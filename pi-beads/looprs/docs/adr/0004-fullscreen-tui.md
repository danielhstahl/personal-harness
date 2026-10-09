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

**R22. A full-screen child that asks for the alternate screen does not get it; it gets a canvas.**
Rule R1 says the alternate screen is ours, and a child typing `?1049h` is asking for the same
resource. The rule is that **we keep it**: the child's enter and leave are cut out of the byte
stream, a blank canvas (`\x1b[H\x1b[2J`) is handed over in place of the enter, and the takeover
and release are reported to the UI exactly as if the switch had happened. Full decision, the
asymmetry that decides it (tee the enter and the user's saved main screen *is* our frame, so
their scrollback is destroyed before anything could be corrected), and the take-back
re-assertion list are ADR-0001 amendment 4 — recorded there because the mechanism lives in the
Bash screen path, and restated here because **R1 is the rule that makes the collision exist** and
must not be read without it. Three consequences worth saying in this file's own voice:

* **The user never leaves our screen.** Across a whole vim session the wire carries one
  `?1049h` (ours, at startup) and no `?1049l` until the exit's own single leave. The handover
  is total at the pixels and invisible in the mode ledger.
* **The hand-back re-asserts every mode we hold except the two that must not be re-asserted** —
  `Raw`, which is a syscall, and `AltScreen`, which is the save-the-main-screen sequence. vim
  switches the mouse and bracketed paste off on the way out; without this the session comes back
  with no mouse and pastes that execute.
* **"Our scrollback is intact" now has a stronger meaning than a promise.** Our frames were
  never composited onto the user's main screen, and nothing of the child's was saved over it,
  because between our enter and our exit there was no second screen switch at all.

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
3. ~~**What the user sees while a full-screen child holds our alternate screen**~~ — **settled
   by R22 and `spikes/fullscreen_e2e.py`**: the child gets the whole canvas and the whole
   keyboard, the user is never dropped out of our screen, and the child's own `?1049` enter and
   leave never reach the terminal. What is still open is only what a real human sees at the
   pixels — the pty has no window — so the `--in-terminal` leg of the same spike remains the
   way to check the aesthetics, not the mechanism.
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

## Landed — `pdl.4`, the frame migration, as built

`Mode::DEFAULT` is `[Raw, AltScreen, CursorHidden]`: the app takes the alternate
screen at startup and every cell of the window is the frame's. The four bands tile
the whole window instead of "the room above the pane", and the tiling is a pure
function of the area it is handed.

> **Amended 2026-10-08 (looprs-5o4.6).** The frame has **five** bands now: the
> beads kanban band landed between the tool rows and the status row
> ([ADR-0007](0007-kanban-board.md), user page `docs/kanban.md`), and it is
> **zero rows** in every frame that is not drawing it. "Four bands" above is
> what the frame was at pdl.4; `src/viewport.rs`'s own header is the live
> description of the five, and the fifth is paid for the way described in
> "What a fifth band costs a contributor" below — out of the transcript's
> surplus, last on the ladder, and nothing else's rows.

**What went out, and what replaced it.**

| Gone | Replaced by |
| --- | --- |
| `desired_height` / `max_live` / `preview_limit` | `viewport::bands(tool_rows, input_rows, frame_rows)` — the input box is paid first, the tool wall is cut next, the transcript band keeps `MIN_TEXT_ROWS` and absorbs the rest |
| `fit` / `needs_fit` / `resize_window` / `follow_the_pane_down` / `anchor_lost` / `respawn` | nothing. The frame's area *is* the window, and `ScreenFrame::draw` re-reads it in `Terminal::autoresize` — a size `ioctl` and a clear, not a cursor query |
| `insert_before` (printing the transcript into the user's scrollback) | the transcript band: the tail of `SessionView::display` plus the live preview, pinned to the bottom of the band. R4 needed no further work — the app paints nothing outside the alternate screen at all |
| `LiveAnchor` / `restore_bytes` (teardown knowing the pane's top row to erase from) | nothing. The hand-back is `?1049l`, and there is no pane top row to know. See the note in ADR-0006 |
| `KEEP_SCROLLBACK_ROWS` / `MAX_LIVE_ROWS` (a height policy negotiated against the user's scrollback) | `MAX_TOOL_ROWS` and `MIN_INPUT_ROWS..=MAX_INPUT_ROWS`: a height policy about *bands* — how tall is the input box, how many tool rows fit — with nothing left to negotiate against |
| the `ESC[6n` cursor query, and the five `drop(keys); keys = EventStream::new()` stop/restart pairs around it | one `EventStream`, started once and left started. The frame never asks the terminal a question, so the key stream never has to get out of the way |

**Measured, not asserted.** `src/viewport.rs` production code 603 → 278 lines
(−54%); whole file 1533 → 757. The run loop in `main.rs` 387 → 282 lines, run
loop + `view()` 434 → 341. `desired_height`, `needs_fit`, `max_live`,
`preview_limit`, `follow_the_pane_down`, `anchor_lost`, `KEEP_SCROLLBACK_ROWS`,
`MAX_LIVE_ROWS`: 0 occurrences each. Key-stream stop/restart pairs 5 → 1, and the
one that remains is the quit path's, kept because stopping the reader costs nothing
and makes the no-query rule independent of whoever edits that list next. Cursor
queries: 0 in the frame and 0 on the exit path, proved by a `Probe` backend that
counts `get_cursor_position` calls and fails the test if the frame makes one
(`viewport::tests::the_frame_never_asks_the_terminal_where_the_cursor_is`).

**What a fifth band costs a contributor** — three places, in this order. The
kanban band (looprs-5o4) is the fifth band this describes, and the list below is
that build's corrected version:

1. `viewport::bands` — grant the rows and say who pays. The ladder is the entire
   policy: the input box is paid in full, the tool wall gives way before it, the
   transcript absorbs what the ladder did not spend, and `MIN_TEXT_ROWS` is the
   floor nothing goes below. **The one correction the fifth band forced:** a band
   that ranks *below* the transcript's floor must not be a term inside `bands` at
   all, or "the board shrank my input box" becomes representable. The board is
   granted afterwards, out of the surplus `bands` leaves, by
   `viewport::kanban_rows` — so the ladder as built is the box > the cards > the
   transcript's floor > the board.
2. `viewport::frame_areas` — one more `Constraint` and one more entry in the
   returned value. This is the only place the tiling is written, and it is pure,
   so a band that is the wrong size is a bug in `bands`, never in the layout.
   Updated since pdl.4: the returned value is a **named** `FrameAreas` struct
   rather than an array, so the compile-checked thing at a draw site is the band's
   *name* (`areas.kanban`) and not an index whose meaning nobody can read.
3. `main.rs::view` — draw into the area `frame_areas` handed back. The band does
   not know the window and the frame does not know what the bands contain.

And a fourth thing that is not code: a band the operator can see, or turn off,
gets documented where the other knobs already live — `docs/kanban.md` for the
board, plus the ADR that decided it. A second list of the same knobs is a
future disagreement.

The tests that keep that ordering honest are
`viewport::tests::the_bands_tile_the_window_at_every_size` (above the
affordability line every band gets exactly what `bands` granted it; below it,
nothing runs off the bottom edge) and
`viewport::tests::the_input_box_outranks_the_tool_wall`.

**Spike premises that had to change.** Every one of these was updated in place,
with the reason recorded in the spike, because each of them was measuring the
inline pane:

* `flash_e2e.py` — was: time the hole between the pane's `ESC[J` and the bytes
  that replace it. The frame has no such step; it diffs the whole screen and
  writes once per frame. It now asserts **zero** partial erases on the wire,
  times any `ESC[2J` (the full repaint a resize or a returned full-screen child
  forces) against the same 1.5 ms budget, and carries two non-vacuity checks so a
  run that did nothing cannot pass for a measured one. The control inverted: the
  pre-pdl.4 binary fails it with **26 partial erases, 1.83–3.58 ms holes**
  (`spikes/results/flash-e2e-pdl4-control.log`); the migrated binary passes
  **4/4** (`spikes/results/flash-e2e.log`).
* `shutdown_e2e.py` — the erase-anchored checks ("the live pane is erased", "the
  live tail landed above the erase line", "one closing newline after the erase")
  measured a pane that no longer exists. In their place: nothing is painted before
  the app takes the alternate screen; no pane-erase shape appears anywhere in the
  run; the answer was painted on the screen the frame left; and **nothing at all
  is written after the `?1049l`** — the closing newline belonged to the inline
  pane's last row, and after the leave the cursor is the user's prompt's cursor.
  149 (`spikes/results/shutdown-e2e-pdl3.log`) → 153 checks
  (`spikes/results/shutdown-e2e.log`), all passing; the same spike on the tree
  this site documents prints 171
  (`spikes/results/shutdown-e2e-00u23.log`).
* `status_e2e.py` — "nothing was flushed to the alt screen" asserted the old
  premise head-on. It now asserts the app took the alternate screen **exactly
  once**, because a second `?1049h` mid-run re-saves the user's own contents as
  their main screen — the one thing this app must never do to their scrollback.
* `bash_e2e.py` — "the command line itself is echoed as typed" greps the wire,
  and a diffed frame legitimately does not re-send a cell that already holds the
  glyph being asked for: the `i` in `echo hi` was elided because the previous line
  had put an `i` in that cell. That check now reads a reconstructed screen
  (`status_e2e.Screen`, the same VT model the status spike already needed for
  exactly this reason) rather than the stream.
* `viewport_e2e.py` and `cancel_e2e.py` needed no change: their needles are "is
  the text on screen, and in time", and survive the migration as written
  (16/16 and 19/19). `fullscreen_e2e.py` (70/70) is unchanged too, which is the
  point of ADR-0001 amendment 4's cut: the child gets the frame's screen and the
  app's screen is never lost.

## Landed — `pdl.6`, the scrollback store, as built

R14's mandatory provenance is one struct, `state::scrollback::DisplayRow`, and
each field exists because a later ticket asks a question of it that nothing else
can answer:

| field | what it answers | who asks it |
| --- | --- | --- |
| `entry` | which `Transcript` entry the row was rendered from. Re-based when the view buffer compacts (`entries_evicted`) — the one way it can go stale, and the one way it is kept honest | pdl.9: a selection must not cross a mode boundary, and a card's chrome is not a row at all, so there is nothing there to select |
| `logical` | which logical line within that entry — the run of rows ending in a hard newline, i.e. the unit the *source* wrote, as opposed to the rows our wrap cut it into | pdl.10: where one pasted paragraph ends |
| `start` | byte offset of this row's first character within that logical line's rendered text (`0` on a line's first row) | pdl.9: the character a cell range maps to, when the selection starts mid-wrap |
| `end: RowEnd` | `Hard` — the source had a newline here; `Soft` — this is our wrap's continuation | pdl.10: soft joins insert **nothing** (the space the wrap broke on is already gone), hard joins insert exactly one `\n`. Implemented as `paste_text`, tested as `joining_rows_follows_the_hard_soft_rule` |
| `cells: CellMap` | the row's clusters laid out in cells: `CellSpan { cell, cells, start, end }`, in *cluster* units, not bytes or code points | pdl.9: snap a cell range to character boundaries, so a copy never carries half a CJK glyph or half a ZWJ family |

The pdl.5 decision ("decode SGR, keep the styles") is honoured the way that
decision asked to be honoured rather than half-implemented: `line` is a ratatui
`Line` with its spans, the row paints styled, and the copy path never sees any
of it. `styles_render_and_are_never_copied` is that rule as a test — the same
row that paints yellow lands in `paste_text` as plain text.

**The scroll state** rides in the same struct: `offset` (rows hanging between
the bottom of the view and the tail), `pinned` (`offset == 0`, cached and
written only by `set_offset` so the two cannot disagree), `pending` (rows that
arrived while unpinned — the "N new" number and nothing else), `width`, and a
`max_rows` cap whose `dropped` count makes the trim a counted thing instead of
an invisible one (pdl.7 reads it). There is deliberately **no absolute scroll
position**: every question the frame asks is relative to the tail, which is the
only end of this content that moves.

**The re-wrap is anchored on content, not on the row index.** `rewrap` records
the row the view is resting on as a `ContentAnchor { entry, logical, byte }`
before the re-render and finds it after, so a window drag keeps the sentence
under the reader's eye instead of jumping the text. When the anchor's entry has
been trimmed away the store *holds* rather than inventing a position
(`rewrap_with_lost_content_holds_rather_than_inventing_a_position`): a guessed
position is a lie about where the user was.

**The "N new" affordance is an overlay, not a row**, for exactly the reason R21
gives for the copy toast. An affordance that reserves a row reshapes the text
below it on every arrival — the thing the user stopped to read would jump each
time the count ticks. `NewRowsPill` covers cells on the band's bottom row, the
row the tail would be on, and is gone at a count of zero (`shows_new`).

**What the next tickets inherit in code, not only in rules.**

| call | who calls it |
| --- | --- |
| `window(visible)` — the slice the offset says | `TranscriptBand` draws that and nothing else; it takes `&[DisplayRow]` now, so the band has no second, shallower copy of the transcript to keep track of |
| `scroll_by(delta, visible)` — positive is toward the tail | pdl.8's wheel passes a flick-sized delta; pdl.13's keys pass a page; both are the same call, which is what keeps wheel and keyboard from drifting apart |
| `scroll_to_tail()` | the one action the pill names |
| `entries_evicted(removed, notice_at)` | pdl.7's file route starts from rows whose provenance is still true; the test that a byte trim cannot break it is `eviction_leaves_no_row_the_transcript_cannot_re_render` |
| `paste_text(rows)` | pdl.10's copy path; the hard/soft rule is already in it, so the ticket cannot forget the rule by forgetting to write it |

**Measured.** `src/state/scrollback.rs` is 1077 lines (store plus tests). Unit
tests 437 → 471. `MAX_DISPLAY_LINES` is gone, `SessionView::display: Vec<Line>`
is `scrollback: Scrollback`, and `App::transcript()` became `App::scrollback()`
plus `App::transcript_window(visible)`. The run loop's flush still happens in
the branch that draws, and still with the width that frame will draw at — what
changed is that its output goes into a store with an offset instead of a vector
that is always shown from the end. One new real-pty spike:
`spikes/scrollback_e2e.py`, **23/23**.

## Landed — `pdl.15`, the window is read, not heard

The `pdl.6` entry closed with an admission: an **idle** app produced **zero bytes**
for two seconds after a `TIOCSWINSZ`, so a window drag on a quiet session left
the previous frame — with the transcript wrapped for a width that no longer
existed — until something else made the app draw. `pdl.6` filed the discrepancy
instead of explaining it. This is the explanation, and then the change.

**The measurement was a harness artifact, and only half of one.** `SIGWINCH` is
not delivered to the process that is drawing on a terminal. It is delivered to
the **foreground process group of that terminal's session** (`tty_ioctl(4)`).
Every spike in this directory spawns the app with `Popen(stdin=slave)` and never
makes the pty the child's controlling terminal, so the signal went nowhere and
the app heard nothing about a window it was being asked to repaint. Run the
identical check against a child that did `setsid()` + `TIOCSCTTY`
(`spikes/resize_e2e.py`, group `attached`) and the **pre-ticket binary repaints**.
The bug was never `App::set_window`, never the `Event::Resize` arm, and never
the `dirty` gate.

It is still a bug, and the fix is not the one the ticket guessed ("force a
repaint on `Resize`" — that already happened). Two halves:

* **one delivery channel is not a design.** The app learned its own window from a
  kernel signal, through a library event stream, onto a flag — none of it ours,
  all of it load-bearing. A channel that can be silent is a channel that will be
  silent: coalesced, blocked, swallowed, or, as here, never sent at all.
* **the symptom is real whenever that channel is quiet**, and a real terminal only
  hides it for as long as the chain holds.

`viewport::WindowPoll` closes it at the root. Once per ~16 ms tick the run loop
asks `TIOCGWINSZ` — which describes the descriptor this process already holds,
with no process group, no signal and no emulator in the way — and adopts the
answer when it differs. **The `ioctl` is the authority on the window;
`Event::Resize` is a notice that says "look now", and it stays for its
latency.** The cost is one syscall per frame already painted: it is the same
call ratatui's `autoresize` makes inside `draw`.

**This is not the poll `pdl.4` deleted.** That one existed because a resize
landing while the key stream was stopped around an `ESC[6n` cursor query was
never delivered at all, so the size had to be re-asked every tick to make up for
a stream that kept being taken away. This poll has nothing to do with the cursor,
which is never queried; it exists because the *signal* is not ours to receive.
The distinction is worth keeping: if the event stream is ever proven to deliver
everything, the event stays (it is faster) and the poll becomes belt-and-braces
at one microsecond a frame.

| | before | after |
| --- | --- | --- |
| how the app learns its window | `SIGWINCH` → crossterm `EventStream` → `Event::Resize`, or nothing | the `ioctl` every tick, plus the event for latency |
| a drag with no signal delivered | the old frame, indefinitely; a full-screen child left at its old size | repainted within about one tick (**7.6 ms** in the committed run's bare harness), children resized |
| ways to adopt a window | two, divergent: `App::set_window`, and an inline `self.width = w; self.height = h` in `Msg::Term(Event::Resize)` that told the children nothing and set no repaint flag | one: every route goes through `set_window` |

The two guards in `WindowPoll` are load-bearing and are the reason it is a type
rather than four lines in the loop. **One unreadable size retires the poll for
the whole run**, because crossterm's `terminal::size()` falls back to spawning
`tput` when the `ioctl` fails and a retrying poll would fork twice a frame; and
**a `0 × 0` is never adopted**, because that is a pty coming apart rather than
a window arriving, and adopting it would forward a zero-sized window to every
child pty.

**Raw shell lines are still not re-wrapped, and that is not this ticket.** The
store re-wraps what it wrapped itself — `Answer`, `Thinking`, `User`, `Error`.
A `MessageKind::Bash` row is ended by the *child*, and ADR-0001 rule 1 as
re-made by ADR-0005 forbids re-wrapping it, so a shell line longer than the
window is cut at the right edge at every width, before and after this change.
Turning that cut into a wrap is a decision about ADR-0005 and not a resize fix;
`spikes/resize_e2e.py` says so out loud in its `rewrap` group rather than
testing only the prose and letting the contrast go unrecorded.

**Measured.** `spikes/resize_e2e.py`, **53/53** across six groups. Control:
the same script against the pre-ticket binary, **0 of its 5 repaint-claim checks
pass** — nothing repaints in the bare harness at all, and the held full-screen
child's own `stty size` still says the old size. On the fix: first byte
**7.6 ms** after the `ioctl` in the committed run — the wait is one tick plus
the draw, never the indefinite stall the ticket describes (5–21 ms across the
runs so far, bare and attached) — the app back to **0 bytes out over 1.5 s**
between drags (the poll is not a repaint loop), and a six-size burst ending at
the **last** size rather than one from the middle.

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
| R1, R3, R4 | `?1049` parks and restores screen + cursor; the ledger hands every mode back **exactly once** across quit / `LOOPRS_MODES=all` / `SIGTERM` / `SIGHUP` / panic-in-draw / killed-child-holds-the-screen — ADR-0006: **149/149** (`spikes/results/shutdown-e2e-pdl3.log`), against **85/112** for the same spike on the pre-ticket binary (`spikes/results/shutdown-e2e-pdl3.log`, `spikes/results/shutdown-e2e-pdl3-control.log`); on the tree this site documents, **171/171** (`spikes/results/shutdown-e2e-00u23.log`) |
| R6, R8, R20, R21 | `clipboard-cost.log`: pbcopy write 8.9–9.7 ms, read-back 9.7–10.7 ms, byte-exact at 128 B → 1 MiB |
| R6, R7, R9, R10 | `terminal-matrix.md`: WezTerm OSC 52 byte-exact at every size to **1 MiB**, **0.01 s**, no prompt; Apple Terminal **0 B in 6.01 s**; SSH remote→local **4,117 B whole in 0.13 s**; read-back query answered by nobody (`bytes_back=0`) |
| R7 | `mouse-clipboard-ssh.log`: the hop carries the bytes; without a far-side pty **21/21 bytes arrive and 0 events decode** — the transport is not the problem, the tty is |
| R9 | the only two caps measured are **0 B** and **≥1 MiB**; nothing between, and nothing discoverable from inside the app |
| R12, R13 | burst: **128/128 reports in 0.282 ms**, worst gap 0.036 ms, nothing merged (so the drag redraw is free and the copy is not) |
| R14 | ADR-0005: `line.cells == line.text.width()` asserted per line; the store is control-free; `copy_text()` is the trim rule |
| R19 | `clipboard-cost.log` unit table: 2/4/6, 7/14/21, 5/2/18, 2/1/3 — and per-character widths summed over the ZWJ family (6) disagree with the family's own width (2) |
| R22 | `fullscreen_e2e.py` run twice, inline and `LOOPRS_MODES=all`: **70/70**. The child's `?1049` family never reaches the wire, no leave during the run, the canvas is handed over in its place, mouse/drag/SGR/bracketed-paste are back on after vim switched them off (with a control proving the child really switched them off), no watched mode left changed, a SIGKILLed child owes no leave, and the exit leaves the alternate screen exactly once |
| R18 | `flash-e2e-at-pdl5.log`: a reshape's erase→content hole is 8.8–9.3 ms, so the toast is an overlay |
| R17, open-2 | the shift-drag fall-through is **measured as unmeasurable from a pty** (`5b`), which is why R17 has three hatches and not one |

`./scripts/check.sh` is clean (fmt, `clippy --all-targets -D warnings`, 440 tests, 3 ignored) with
the new example added and nothing else touched.

## Landed — `pdl.13`, the copy chords, as built

Keyboard parity: `PageUp`/`PageDown`/`Home`/`End` in all three modes on the
wheel's own pin/unpin semantics (they were already the same store — `App::scroll_active`,
`top_active`, `tail_active` — so this ticket *proved* the parity rather than
building it), and a copy family that needs no mouse.

**Why one leader chord instead of four chords.** The keyboard was already spoken
for: `Ctrl-C`, `Ctrl-Q`, `Tab`, `Shift-Tab`, `Enter`, `Shift-Enter`, `Esc`, the
four page keys, and every printable. Four more top-level control chords would have
meant either stealing from a child terminal's own set or picking Alt chords, whose
delivery is a per-emulator lottery. So `Ctrl-S` **arms** a window and the next key
picks the target:

| key | target | where it works |
|---|---|---|
| `Ctrl-S a` | the last answer | Pi, Beads (refused in Bash, naming `Ctrl-S o`) |
| `Ctrl-S o` | the last command's output / last finished tool card | all three |
| `Ctrl-S s` | the live selection | all three (mouse-made today — see *not done*) |
| `Ctrl-S t` | the whole transcript, to a file | all three |
| `Ctrl-S ?` | this list | all three |
| `Esc` | lower the chord | all three |

The window is `COPY_CHORD_WINDOW = TOAST_TTL`, deliberately the same number and
not a coincidence: the hint toast that named the window expiring **is** the window
expiring, so there is no moment where the app is waiting and the screen says
nothing. Expiry is silent for that reason.

**`Ctrl-S` is XOFF, and that is the whole hazard.** Raw mode via crossterm goes
through `cfmakeraw`, which clears `IXON`, so looprs receives `0x13` as a key
event instead of the terminal stopping its own output — that is why the chord is
possible at all. The hazard is the other direction: `0x13` **must never be
forwarded to a full-screen child**, because a child stopped with XOFF needs
`0x11` to resume and `Ctrl-Q` is looprs's *quit*. So while a child holds the
screen, `Ctrl-S` is swallowed rather than sent and the chord is not armed
(`a_child_holding_the_screen_is_never_sent_xoff`), and a chord that was armed
before the child took over is dropped (`a_child_taking_the_screen_kills_an_armed_chord`)
rather than left to fire at content the user can no longer see.

**The command boundary is made, not inferred.** A Bash session is one stream of
one `MessageKind`, so "the last command's output" has no natural edge: the seal
is taken at submit (`SessionView::seal_shell_output` → `Transcript::seal_command`)
and records the entry index the command's output starts at. Reading *that block*
rather than "the last Bash entry" is not taste. A real pty ends a command with a
blank line of its own — the next prompt's carriage return — with looprs's own
`exit 0` note breaking the shell stream behind it, so "the last Bash entry" was
the blank and `Ctrl-S o` answered *"the last command's output is blank"* about a
screen full of the command's output. Found by running the real binary in tmux
(`spikes/tmux_keyboard_e2e.py`), not by the unit tests, which had been pushing
shell output without the artifact. Three facts now stay separate: no boundary
("no command has run"), a blank block ("produced no output"), and a copy.

**The dump toast has to be readable, which moved the default directory.** The
escape hatch writes to `$XDG_CACHE_HOME/looprs`, else `$HOME/.cache/looprs`,
else the temp dir — because the toast has to *name the file* on a row the user can
read, and macOS's `$TMPDIR` is 49 columns of directory before the name starts.
Through tmux at 100 columns the toast came out cut mid-path: a toast whose whole
job is "here is your file" that names nothing. The path is folded at `$HOME`
(`~/.cache/looprs/looprs-pi-…txt`), the absolute path is what is actually written
and what the startup `info` log carries. The verb is `Wrote`, never `Copied`,
because the clipboard was not touched and a user who reads `Copied 38,000
characters` and pastes gets whatever they had before.

**The three refusals that are not silence.** A blank *mouse* selection stays quiet
(R13: it was never a request). A keyboard copy that finds nothing is **loud** —
the user pressed keys on purpose and is owed a reason — and the reason names the
missing thing, and usually the chord that would have worked.

### tmux and SSH, measured (`spikes/tmux_keyboard_e2e.py`, 37/37)

The first spike of this kind in the repo: the app driven inside a live `tmux`
3.7c session rather than a pty the harness owns. Fakes for `pi` and `bd` are
mandatory, not tidiness — looprs boots into Beeds and the beads loop runs
`bd update <id> --claim` before a Tab can reach it, so an unplanned
`looprs` run moves the real board (it did, once, during this ticket; restored).

- **`Ctrl-S` arrives.** tmux puts its client in a `cfmakeraw`-equivalent mode, so
  `0x13` reaches the app as a key event: the arming hint paints. tmux has no
  binding on `C-s`, and the app does not care if it did — the pane gets the byte.
- **The page keys survive the hop** (`ESC[5~`/`ESC[6~`/`ESC[H`/`ESC[F`): `Home`
  reaches row 1 of a 200-row transcript, `End` reaches row 200, and a `PageDown`
  taken at the tail does not move — the pin `End` set.
- **`Ctrl-C` is still the child's**: the interrupt round-trips in ~0.04 s and no
  `Copied` toast appears anywhere on screen.
- **A full-screen child is never XOFFed**: after our `Ctrl-S`, vim is driven into
  insert mode and repaints (`NOT-XOFFED` on the top row). The check that matters,
  because the failure mode is unrecoverable from inside the app.
- **The alternate screen is clean through tmux**: `#{alternate_on}` is `1` while
  looprs runs and `Ctrl-Q` closes the pane.
- **The boundary is real from outside the process**: two same-shaped commands
  copied back to back give **66 and 62 characters**, not 66 and ~130.
- What tmux *cannot* test is a real drag: `send-keys` injects SGR reports through
  the pane's input, which proves our parser and not the multiplexer's forwarding.
  R17's shift-drag fall-through stays unmeasurable from a pty, as this ADR said
  before the ticket started.

### Not done, said out loud

**Keyboard selection did not land.** `Ctrl-S s` copies the *live* selection,
which today means one the mouse made; the ticket's "if a keyboard selection lands"
is the hatch this stopped at. What was built instead is the seam: the target, the
resolution, the refusal ("nothing is selected — `Ctrl-S a` copies the last
answer, · `Ctrl-S o` the last output") and the same sink and toast the mouse uses,
so a keyboard selection is a producer plugged into a target that already exists.

`./scripts/check.sh` is clean (fmt, `clippy --all-targets -D warnings`, 636 tests,
3 ignored), plus 37 tmux checks that the unit tests cannot make.
