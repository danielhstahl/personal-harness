# Keymap and chords

Every keystroke looprs claims, per mode, generated from the same table the app's own
rule tests read: [`CHORD_TABLE`](../../src/session/view/chord_table.rs) — a typed row per chord
with `mode`, `key`, `keys`, `state`, `owner`, `does` and a `note`. The tables below
are **generated from that table** (see [generated, not typed](#generated-not-typed)),
so they cannot disagree with the binary they describe.

Three things to know before reading the tables:

1. **There is no "all modes" row.** The source table deliberately has one row per
   mode per chord because `Ctrl-C` does not mean the same thing in Bash as in
   Pi/Beads. Anything that flattened that would be hiding the only surprising row.
2. **`Ctrl-Q` is the exit.** It quits in every mode, in every state, and it owns
   the child on the way out. `Ctrl-C` is a different key in Bash and that is not an
   oversight.
3. **Copy is a chord family behind one prefix** (`Ctrl-S`), not four top-level
   chords, because the chord budget in a terminal is the entire difficulty — see
   [the copy family](#the-copy-family-ctrl-s).

**A note on the spelling, because it used to cost people time.** The mode is
**Beads** everywhere: `TerminalType::Beads` in the code, `label() == "Beads"` on
the status row, **Beads** in the prose here. The generated tables below use the
variant name for the rows, so what a table row says is what the screen says.

---

## Beads mode

<!-- BEGIN GENERATED:keymap:beads -->
| key | state | who owns it | effect | why |
| --- | --- | --- | --- | --- |
| `Ctrl-C` | Plain | app | `Quit` | same as Pi |
| `Ctrl-Q` | Plain | app | `Quit` | quit in every mode |
| `Ctrl-S` | Plain | app | `ArmChord` | the copy prefix, armed |
| `Ctrl-S a` | ChordArmed | app | `CopyAnswer` | the last answer, whole, through the looprs-pdl.10 sink and toast |
| `Ctrl-S o` | ChordArmed | app | `CopyLastOutput` | the agentic modes' answer to the same question: the last finished tool card |
| `Ctrl-S s` | ChordArmed | app | `CopySelection` | whatever selection is live |
| `Ctrl-S t` | ChordArmed | app | `DumpTranscriptFile` | the escape hatch |
| `Ctrl-S ?` | ChordArmed | app | `ChordHelp` | the family, listed in a toast |
| `Esc` | ChordArmed | app | `CancelChord` | undoes the prefix and nothing else |
| `Esc` | SelectionLive | app | `ClearSelection` | was a selection live? yes \u{2014} the first Esc unselects and sends nothing |
| `Esc` | Plain | session | `CancelRun` | was a selection live? no \u{2014} the cancel |
| `Tab` | Plain | app | `SwitchMode` | switch mode |
| `Shift-Tab` | Plain | box | `Newline` | newline in the input box |
| `Shift-Enter` | Plain | box | `Newline` | a newline in the box |
| `Enter` | Plain | box | `Submit` | submit |
| `PageUp` | Plain | app | `ScrollUp` | one page up, unpinning the tail |
| `PageDown` | Plain | app | `ScrollDown` | one page down; reaching the bottom re-pins |
| `Home` | Plain | app | `Top` | top of the transcript |
| `End` | Plain | app | `Tail` | bottom, and re-pin |
| `anything else` | Plain | box | `Typing` | typing |
<!-- END GENERATED:keymap:beads -->

## Pi mode

<!-- BEGIN GENERATED:keymap:pi -->
| key | state | who owns it | effect | why |
| --- | --- | --- | --- | --- |
| `Ctrl-C` | Plain | app | `Quit` | quit for now: Pi mode has no Cancel worth the name until looprs-5g7, and a copy chord is not what Ctrl-C becomes |
| `Ctrl-Q` | Plain | app | `Quit` | quit in every mode |
| `Ctrl-S` | Plain | app | `ArmChord` | the copy prefix, armed |
| `Ctrl-S a` | ChordArmed | app | `CopyAnswer` | the last answer, whole, through the looprs-pdl.10 sink and toast |
| `Ctrl-S o` | ChordArmed | app | `CopyLastOutput` | the agentic modes' answer to the same question: the last finished tool card |
| `Ctrl-S s` | ChordArmed | app | `CopySelection` | whatever selection is live |
| `Ctrl-S t` | ChordArmed | app | `DumpTranscriptFile` | the escape hatch |
| `Ctrl-S ?` | ChordArmed | app | `ChordHelp` | the family, listed in a toast |
| `Esc` | ChordArmed | app | `CancelChord` | undoes the prefix and nothing else |
| `Esc` | SelectionLive | app | `ClearSelection` | was a selection live? yes \u{2014} the first Esc unselects and sends nothing |
| `Esc` | Plain | session | `CancelRun` | was a selection live? no \u{2014} the cancel, and the queued text comes back to the box |
| `Tab` | Plain | app | `SwitchMode` | switch mode |
| `Shift-Tab` | Plain | box | `Newline` | newline in the input box |
| `Shift-Enter` | Plain | box | `Newline` | a newline in the box |
| `Enter` | Plain | box | `Submit` | submit |
| `PageUp` | Plain | app | `ScrollUp` | one page up, unpinning the tail |
| `PageDown` | Plain | app | `ScrollDown` | one page down; reaching the bottom re-pins |
| `Home` | Plain | app | `Top` | top of the transcript |
| `End` | Plain | app | `Tail` | bottom, and re-pin |
| `anything else` | Plain | box | `Typing` | typing |
<!-- END GENERATED:keymap:pi -->

## Bash mode

Bash is the mode where the app is a guest in someone else's terminal: `Ctrl-C` is
the shell's key, and a full-screen child can take the whole screen. The Bash table
has rows the other two do not, and they are the interesting ones.

<!-- BEGIN GENERATED:keymap:bash -->
| key | state | who owns it | effect | why |
| --- | --- | --- | --- | --- |
| `Ctrl-C` | Plain | shell | `SigInt` | the shell's own key: 0x03 to the pty master, the foreground group gets SIGINT, the app stays up — and it is never, in any mode, a copy |
| `Ctrl-Q` | Plain | app | `Quit` | quit without touching the shell; this is also why Ctrl-S cannot be forwarded \u{2014} \u{2014} \u{2018} is ours |
| `Ctrl-S` | Plain | app | `ArmChord` | the copy prefix: a prefix rather than four top-level chords, because the chord budget is the whole difficulty here |
| `Ctrl-S a` | ChordArmed | app | `CopyAnswer` | in Bash this *refuses* (there are no answers here) and names the chord that works |
| `Ctrl-S o` | ChordArmed | app | `CopyLastOutput` | the last sealed shell block: this command's echo, output and prompt, and nothing before it |
| `Ctrl-S s` | ChordArmed | app | `CopySelection` | whatever selection is live; a keyboard-driven selection was *not* one of the things that landed in this ticket, and this target is ready for it |
| `Ctrl-S t` | ChordArmed | app | `DumpTranscriptFile` | the escape hatch: the whole transcript to a timestamped file, through an injected sink like the clipboard's |
| `Ctrl-S ?` | ChordArmed | app | `ChordHelp` | the family, listed in a toast: the chords have to be reachable from inside the app |
| `Esc` | ChordArmed | app | `CancelChord` | undoes the prefix and nothing else \u{2014} it is deliberately not the mode's cancel |
| `Ctrl-C` | ChildHolds | shell | `SigInt` | still SIGINT: a full-screen child does not take Ctrl-C away from the shell it is already in |
| `Ctrl-Q` | ChildHolds | app | `Quit` | ours in every state; the child is killed on the way out |
| `Ctrl-S` | ChildHolds | app | `Swallowed` | never forwarded: XOFF into a pty whose XON (Ctrl-Q) we own is a freeze the user cannot undo |
| `anything else` | ChildHolds | child | `Forwarded` | a program that owns the screen owns the keyboard (ADR-0001 Q2) \u{2014} Esc included, so vim leaves insert mode |
| `Esc` | SelectionLive | app | `ClearSelection` | was a selection live? yes \u{2014} the first Esc unselects and sends nothing |
| `Esc` | Plain | session | `CancelRun` | was a selection live? no \u{2014} the Esc is the cancel the mode table already describes |
| `Tab` | Plain | app | `SwitchMode` | switch mode; the selection, the chord and the wheel throttle all clear with it |
| `Shift-Tab` | Plain | box | `Newline` | newline in the input box — Shift-Enter never sends a shell command |
| `Shift-Enter` | Plain | box | `Newline` | a newline in the box, not a submit |
| `Enter` | Plain | box | `Submit` | submit; in Bash this is also where the command boundary is sealed for Ctrl-S o |
| `PageUp` | Plain | app | `ScrollUp` | one page up, unpinning the tail (looprs-pdl.8's semantics, the same store) |
| `PageDown` | Plain | app | `ScrollDown` | one page down; reaching the bottom re-pins |
| `Home` | Plain | app | `Top` | top of the transcript |
| `End` | Plain | app | `Tail` | bottom, and re-pin: what the \u{201c}N new\u{201d} affordance names |
| `anything else` | Plain | box | `Typing` | typing, to whichever mode's box is up |
<!-- END GENERATED:keymap:bash -->

**The `ChildHolds` state is the whole story of Bash.** When a full-screen program
(`vim`, `less`, `htop`) is holding the screen, the app forwards everything except
its own control chords, and the forwarding rules are asymmetric on purpose:

| key | while a child holds the screen | why |
| --- | --- | --- |
| `Ctrl-C` | still SIGINT to the shell's group | a full-screen child does not take `Ctrl-C` away from the shell it is already in |
| `Ctrl-Q` | ours; the child is killed on the way out | there has to be one key that always works |
| `Ctrl-S` | **swallowed, never forwarded** | XOFF into a pty whose XON (`Ctrl-Q`) we own is a freeze the user cannot undo. This row exists because that bug is otherwise inevitable |
| anything else | forwarded verbatim | the child gets its keys, including `Esc` for vim's insert mode |

Raw bytes reach the child exactly as typed
([`Session::send_bytes`](../../src/session/mod.rs)): `0x1b` reads as `Esc`, `0x0d` as
Enter. A line-oriented `send_text` would send neither, which is why the two are
different methods and why a mode that cannot take raw keys refuses rather than
ignoring.

---

## The copy family (`Ctrl-S`)

One prefix, five targets, a bounded window. `Ctrl-S` **arms** the chord; the next
key picks the target. Nothing is copied by the prefix alone.

| chord | copies | in Beads / Pi | in Bash |
| --- | --- | --- | --- |
| `Ctrl-S a` | the last answer | the last completed answer, whole, through the clipboard sink, confirmed by toast | **refuses**, and names the chord that works — there are no answers in a shell |
| `Ctrl-S o` | the last output | the last finished tool card | the last **sealed** shell block: that command's echo, output and prompt, nothing before it |
| `Ctrl-S s` | the live selection | whatever the mouse (or a keyboard selection) has selected right now | same |
| `Ctrl-S t` | the transcript dump | the whole settled transcript to a timestamped file, path in the toast | same |
| `Ctrl-S ?` | the cheat sheet | the family, listed in a toast | same |
| `Esc` (armed) | nothing | cancels the chord and nothing else — never a run cancel while armed | same |
| anything else (armed) | nothing | the chord closes without a target and says so | same |

Three properties worth having in mind when a copy does not do what you expected:

* **The toast count is the count that landed.** `Copied 1.2k` describes the bytes
  that reached the transport, not the bytes that were requested. A partial copy
  reports the partial number in failure's colour
  ([ADR-0004 R19](../adr/0004-fullscreen-tui.md)).
* **A refusal names a way out.** "Nothing copied: X is not a copy chord — `Ctrl-S ?`
  lists them." A refusal that only says no is a dead end, and dead ends in a chord
  system get blamed on the app rather than on the keystroke.
* **The target is a semantic unit, not a rectangle.** `Ctrl-S o` in Bash is the
  last *command*, which is why it survives the command being 400 lines long and why
  it will never include the command before it.

The transport (native helper vs OSC 52) is decided once per run and is documented
in [the configuration reference](configuration.md#clipboard-and-mouse), not here.

## `Esc`, per mode

`Esc` is the cancel key, and what it cancels is per mode. Full reasoning in
[ADR-0003](../adr/0003-cancellation.md); the short form:

| mode | `Esc` cancels | how it is announced |
| --- | --- | --- |
| Bash | the shell's foreground command (`0x03` → SIGINT to the foreground group) | the command's own report; the app never says "cancelled" for something it did not do |
| Bash, command queued but not started | the queue entry, and says the command never ran | the transcript says so; the queue entry is taken out rather than fired late |
| Pi | the queued messages first, then `abort` on the run | the transcript gets the abort line; a `pi` that ignores the abort gets killed and the mode recovers |
| Beads | the pass: `abort` the worker, kill it if it ignores the grace | the row goes `cancelling`, then back to `awaiting input`; the ticket stays claimed until the unwind finishes |
| any mode, selection live | the selection | the highlight clears, nothing else |
| any mode, `Ctrl-S` armed | the chord | the chord is disarmed; a run is never cancelled by this `Esc` |

The rule underneath all of them: **`Esc` cancels the nearest thing the app can
actually cancel, and never lies about having cancelled something further away.**

## The mouse

| gesture | effect |
| --- | --- |
| press + drag | starts a selection. It survives a soft wrap, CJK clusters and combining marks — the cell map never splits a cluster |
| release | ends the selection. With copy-on-select on (default), it copies |
| `Esc` with a selection live | clears it |
| scroll wheel / trackpad | scrolls the transcript, throttled so a flung trackpad stays readable |
| buttons the app does not use | reported as doing nothing rather than silently swallowed — a mouse event that vanishes is a bug report with no evidence |

**Copy-on-select** is the automatic path: `LOOPRS_COPY_ON_SELECT=0` turns that path
off completely and leaves the explicit chords working
([ADR-0004 R17](../adr/0004-fullscreen-tui.md)). **Mouse off entirely:**
`LOOPRS_MODES=-mouse` — with the mouse off, the emulator keeps its own selection
behaviour, which is what you want inside tmux. `Ctrl-S s` still copies whatever a
terminal-native selection gave you.

There is no mouse-specific variable. The switch is the mode list.

## The cheat sheet

One screen, printable, 120 columns, no scrolling:

```text
  ┌───────────────────────────────── looprs · keymap ─────────────────────────────────┐
  │ MODES          Tab                next mode:  Beads → Pi → Bash → Beads             │
  │                (a Tab away)       Bash/Pi keep running · Beads drains, then parks    │
  │                                                                                    │
  │ QUIT           Ctrl-Q             quit, every mode, every state. THE exit key.       │
  │ INTERRUPT      Ctrl-C   (Bash)   0x03 to the shell — app stays up. Never a copy.   │
  │                Ctrl-C (Pi/Beads) quit                                                          │
  │                Esc      (run)    cancel the run/queue: pass, abort, SIGINT per mode  │
  │                                                                                    │
  │ SCROLL         PageUp / PageDown one page; PageUp unpins, hitting bottom re-pins    │
  │                Home / End        top of transcript / bottom + re-pin                 │
  │                wheel             same, throttled (trackpad momentum is ~100Hz)       │
  │                ▲ N new           you are off the tail; End gets back to it           │
  │                                                                                    │
  │ COPY           Ctrl-S            ARM the copy chord (nothing copied by this alone)   │
  │                  a               last answer          (refuses in Bash, says why)     │
  │                  o               last output: tool card / sealed shell block          │
  │                  s               live selection                                     │
  │                  t               whole transcript → timestamped file                │
  │                  ?               list this family in a toast                          │
  │                Esc  (armed)      cancel the chord only — never the run               │
  │                                                                                    │
  │ INPUT          Enter             send                                               │
  │                Shift-Enter       newline in the box (never a submit)                 │
  │                Shift-Tab         newline in the box                                   │
  │                anything else     typing                                             │
  │                                                                                    │
  │ MOUSE          drag              select (wrap-safe, cluster-safe)                    │
  │                release           copy if copy-on-select is on (default)              │
  │                Esc               clear selection                                    │
  │                off               LOOPRS_MODES=-mouse  (tmux: you want this)          │
  │                                                                                    │
  │ CHILD HOLDS    vim/less take the screen in Bash: all keys forwarded EXCEPT Ctrl-C   │
  │   THE SCREEN   (still SIGINT), Ctrl-Q (ours, kills child on exit), Ctrl-S           │
  │                (swallowed — XOFF with our XON is an unrecoverable freeze)           │
  └────────────────────────────────────────────────────────────────────────────────────┘
```

## generated, not typed

The three per-mode tables between the `BEGIN GENERATED` / `END GENERATED` markers in
[this file's source](keymap.md) are rendered from
[`CHORD_TABLE`](../../src/session/view/chord_table.rs) by
[`scripts/docs_check.py`](../../scripts/docs_check.py):

```sh
./scripts/docs_check.sh --fix-keymap   # rewrite the tables from the current CHORD_TABLE
./scripts/docs_check.sh                # fail if they disagree (runs in ./scripts/check.sh)
```

Add a row to `CHORD_TABLE` and the build fails until you re-run `--fix-keymap`,
which is the drift control this ticket asked for: the page cannot quietly lag the
table, because the gate diffs the rendered table against the source rather than
trusting that whoever edited one also edited the other.

Prose the table cannot carry lives above: the "why" of the `ChildHolds` XOFF row,
the refusal-named-way-out rule, the semantic-unit property of `o`. Those are not
generated, which is the honest split — the table is the fact, the prose is the
argument, and
[ADR-0004](../adr/0004-fullscreen-tui.md) is the argument behind the argument.

**See also:** [configuration reference](configuration.md) (which knobs change what
a key does) · [ADR-0003](../adr/0003-cancellation.md) (`Esc`) ·
[ADR-0004](../adr/0004-fullscreen-tui.md) (clipboard, R17, R19) ·
[ADR-0001](../adr/0001-bash-terminal-state-pty.md) (the pty a child holds).
