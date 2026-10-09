# Living with a transcript

*Page 3 of 3 in the **"use it effectively"** track. Previous:
[Driving the Beads loop](beads-loop.md) · Start of track:
[Your first session](first-session.md).*

A transcript you can scroll, select, copy, resize and read back after a crash is the
thing looprs actually sells. It is also the thing with the most rules attached. This
page is the user-facing half of
[ADR-0004](../adr/0004-fullscreen-tui.md): what the store promises, where it stops,
and what to do in the four situations where the difference matters.

---

## The pin-to-tail rule

One rule explains almost all of the scrolling behaviour:

> **The view is *pinned to the tail* by default. Scrolling up unpins it. Reaching
> the very bottom re-pins it.**

While pinned, new output moves the screen for you. While unpinned, new output does
**not** move the screen: it accumulates, and the pill tells you how much.

| key | pinned | unpinned |
| --- | --- | --- |
| `PageUp` | unpin, up one page | up one page |
| `PageDown` | n/a | down one page; **the very bottom re-pins and clears the count** |
| `Home` | jump to top, unpinned | jump to top |
| `End` | already there | jump to bottom, re-pin, clear the count |
| wheel / trackpad | same as `PageUp` per notch, throttled | same |

The pill that appears when you are off the tail lives on the transcript band's bottom
row:

```
▲ 37 new · End for the tail
```

That number is what arrived **while you were not looking at the tail**. It is not
"rows below the fold". Coming back with `End` clears it, because the count answers
"how much did I miss", not "how much is below me".

The wheel is throttled on purpose. A trackpad on macOS fires ~100 events a second
with momentum, and an unthrottled transcript on a full-screen child's byte burst is
the failure that
[ADR-0004](../adr/0004-fullscreen-tui.md) measures; the throttle is why a flung
trackpad scrolls the transcript at a readable rate instead of skipping past the
thing you were aiming at.

## Why it is a store and not a scroll

The transcript is an in-memory store (`state::scrollback`) with a byte budget, not
a print stream:

| bound | value | what it means |
| --- | --- | --- |
| retained store, per mode | 32 MiB (`DEFAULT_RETAINED_BYTES`) | the rendered history per mode, with row provenance |
| view buffer, per mode | 256 KiB (`DEFAULT_VIEW_BUFFER`) | the live tail buffer that gets flushed into entries |
| the app's worst case | 3 modes × (32 MiB + 256 KiB) | logged at startup as `retained_ceiling_bytes`; excludes syntect's one-time syntax set |
| the UI's byte lane | `bus::DEFAULT_CAP_BYTES` | in-flight bytes are *paced*, not stored: a producer that outruns the UI waits instead of growing the process |

The last row is the one that saved this app from a real incident: the queue used to
be unbounded, and an instrumented run had ~1.8 M messages (~400 MB) sitting in it
while the UI had consumed 24 MB of what was produced. A bound that *back-pressures
the producer* is the fix; a bigger buffer is the same bug with more RAM. Numbers and
all: [`src/bus.rs`](../../src/bus.rs).

**When the bound bites, it says so.** The marker row reads:

```
⌄ scrollback trimmed: 412 earlier lines dropped
```

and that marker is the honest version of a failure that would otherwise be silent —
you cannot scroll up to something that is not there, so the store has to name the
loss. It also names where the loss went:

> the dropped lines are not lost. They are in the journal file.

## The file behind it

The store is bounded; the **journal** is not. Every entry is appended and flushed to
a file *as it finalises*, on its own writer task, while the session runs — not on
the exit path ([ADR-0004 R2/R3](../adr/0004-fullscreen-tui.md)). That is the whole
reason a bounded scrollback is survivable:

```sh
tail -f ~/.local/share/looprs/transcripts/last
```

works during a run, and the file is there after a `kill -9`, an OOM kill, a panic in
the draw, or a laptop pulled out of power. Full paths, modes, and the per-mode
symlinks: [Files, logs and recovery](operator.md#where-things-are-written).

**It is a reading copy, not a record.** `bd` is the system of record for work. The
journal has no schema, no version and no stability contract, nothing in looprs reads
it back, and a tool built to parse it will be broken by the next release. If a fact
matters, it belongs in `bd`.

## Selecting and copying

Three ways to get text out, and which wins depends on where you are:

| | what it copies | wins when |
| --- | --- | --- |
| **mouse drag** | exactly what you dragged over, across soft-wrapped rows | you are on a local terminal with a working mouse |
| **copy-on-select** | the same, automatically, on button release | you always want the drag's text in the pastebuffer (default on; `LOOPRS_COPY_ON_SELECT=0` off) |
| **`Ctrl-S` chords** | `a` last answer · `o` last output · `t` dump whole transcript to a file | you are in SSH, or the text is *one semantic unit* you did not want to drag across |

The chords are the differentiator. "Copy the last answer" is one keystroke pair
(`Ctrl-S a`) and it is the *answer* — the whole entry, with the chrome off — not
the rectangle you happened to drag over. "Copy the last output" in Bash is the
sealed block for that command: the echo, its output and its prompt, and nothing
before it. Try it after a long `cargo build`: the chord gives you exactly one
command's worth, which is what you were going to spend a drag and three corrections
trying to select. Full family, per mode: [the keymap](keymap.md).

**Over SSH**, `Ctrl-S a` is not a convenience, it is the only path: a native
clipboard helper on the far side writes the *remote* clipboard, which is not where
your paste is. looprs detects the SSH session and uses OSC 52 without being told
([ADR-0004 R7](../adr/0004-fullscreen-tui.md)); if the terminal does not accept OSC
52 the toast says which transport it tried. Overrides:
[`LOOPRS_CLIPBOARD`](configuration.md#clipboard-and-mouse).

**The count in the toast is the count that landed**, not the count that was asked
for. A copy that partially failed says `Copied N of M` in failure's colour rather
than `Copied M`
([ADR-0004 R19](../adr/0004-fullscreen-tui.md)); a refusal names the chord that does
work rather than telling you that nothing happened.

## Resize, and the one thing that does not re-wrap

Resize the window and the transcript **re-wraps**: rows are re-made against the
content the view was resting on, so the line you were reading stays on the screen
rather than the row *number* you were reading staying on screen. That is
`Scrollback::rewrap`, and its nastiest case is a re-wrap whose anchor got trimmed
away, which holds its position rather than inventing one.

**Raw shell output does not re-wrap. At any width. Ever.**

`MessageKind::Bash` blocks are ended by the child, and
[ADR-0001 rule 1](../adr/0001-bash-terminal-state-pty.md) /
[ADR-0005](../adr/0005-shell-output-content-model.md) forbid re-writing them: a
shell's output is its own bytes, laid out against the width the *program* thought
it had, and re-wrapping a `git diff` or a table from `ps` is how you corrupt the
thing you were trying to read. So:

| kind | re-wrapped on resize |
| --- | --- |
| answers, tool cards, prose | yes |
| raw shell output | **no** — cut at the right edge, same as in a plain terminal |

If you need a wide shell table to be readable, make the program produce it narrower
(`COLUMNS=80`, `--no-table`, `| cat`) rather than hoping the app can re-layout
bytes it is not allowed to touch.

## Reading back a session that ended badly

```sh
# the last thing this workspace wrote, any mode
less "$(readlink ~/.local/share/looprs/transcripts/last)"

# the last thing a *specific* mode wrote
less "$(readlink ~/.local/share/looprs/transcripts/last-Beads)"

# and while a session is still running: the live file
ls -lt ~/.local/share/looprs/transcripts/ | head
```

Because the journal is written per entry rather than at exit, this works for the
deaths that matter. The transcript file is plain text in transcript order — exactly
"select everything and copy", which is why it has no timestamps in the body
([ADR-0004 open question 8](../adr/0004-fullscreen-tui.md)).

If you want the transcript *now*, mid-session, without a file you have to go find:
`Ctrl-S t` writes the whole settled transcript to a timestamped file and toasts
the path. The settled transcript only: the live tail has no final form, and the
same reason a drag cannot reach it is why the dump does not either.

## Four things this page is really asking you to remember

1. **Scrolling up unpins; `End` re-pins.** The pill is the difference between "I
   am missing output" and "I am holding my place".
2. **The file has everything.** When the store drops something, the marker names
   the file that kept it.
3. **`Ctrl-S a` beats dragging.** Especially over SSH, where it is not a preference.
4. **Shell output is not re-wrapped, on purpose.** The width you launched it at is
   the width it was laid out at.

**Something not behaving as this page said?**
[Files, logs and recovery → "Debugging a bad run"](operator.md#debugging-a-bad-run).
