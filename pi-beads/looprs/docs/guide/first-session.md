# Your first session

*Page 1 of 3 in the **"use it effectively"** track. The other two:
[Driving the Beads loop](beads-loop.md) · [Living with a transcript](transcript.md).*

This page is about the first ten minutes: what you are looking at when the app
paints, what the row at the bottom is promising you, and how to get out of it from
every state including the bad ones.

Everything described here was run, not recalled. To see the same session without a
model call and without a real board, run the same driver the CI uses:

```sh
cargo build
python3 spikes/status_e2e.py       # 20 checks over 6 scenarios, ~25s
```

---

## Starting cold

```sh
./target/debug/looprs
```

The app opens in **Beads** mode
([`run(&mut frame, TerminalType::Beeds, …)`](../../src/main.rs)). Not because Beads
is the most important mode and not because you asked for it — because the beads
loop's first pass runs during startup, so the input box opens in the right state
instead of flickering once the first message lands. If you came for the shell,
`Tab` twice.

Before anything is drawn, three things happen in a fixed order, and the order is
the point ([ADR-0006](../adr/0006-terminal-mode-ledger.md)):

1. the teardown object is built and the panic hook installed;
2. the terminal modes are switched on **through the ledger** — raw mode, alternate
   screen, hidden cursor, three mouse modes;
3. only then is the frame taken.

So from the first byte the app writes there is an object that knows which modes it
holds and how to hand them back. That is why "the terminal is fine after a crash"
is a property of the program rather than of the exit paths that remembered to
arrange it.

## What the row at the bottom is telling you

The status row is one row, always painted, and it answers four questions:
**is it doing something · what · for how long · did anything fail.** On a cold
start with an empty board you get the idle form of it:

```
● awaiting input · Tab switch · ^C quit
```

Two facts it is promising, in the two tokens that matter:

* **`Tab switch`** — the keystroke that moves you between the three modes is
  `Tab`. One keystroke per hop, fixed cycle Beads → Pi → Bash → Beads
  ([`TerminalType::next`](../../src/session/mod.rs)).
* **`^C quit`** — the keystroke that ends the program. Note the caveat in the
  next section: in Bash `Ctrl-C` is *not* the way out and the row's advice is
  wrong for that one mode, which is why `Ctrl-Q` exists.

The glyph is part of the sentence, not decoration
([`components::status::glyph`](../../src/components/status.rs)): an animated spinner
means *a process is doing something*, `●` means warm and idle, `○` means nothing
is there. A static dot next to a word that says "working" is a bug, and the row's
tests are mostly about exactly that mismatch.

The row also reports the modes you are **not** looking at. Switch to Bash while the
beads loop is mid-pass and the row says `bg: Beads working · looprs-… 12s`. That
is the whole reason the row exists rather than a colour on the input box: after you
`Tab` away from a pass that is costing money, the mode you can see is the one that
has nothing to report
([ADR-0002](../adr/0002-session-abstraction.md)).

## Getting out

| where you are | how out | what happens |
| --- | --- | --- |
| any mode, idle | `Ctrl-Q` | clean exit, ledger hands the terminal back, exit code 0 |
| any mode, mid-run | `Ctrl-Q` | the same, plus the sessions get [`SHUTDOWN_GRACE`](../../src/session/router.rs) to say their piece and then get killed |
| Bash, nothing running | `Ctrl-C` | goes to the shell as `0x03`; **the app stays up**, because that key belongs to the shell |
| Bash, a command running | `Ctrl-C` | SIGINT to the shell's foreground group; the command dies, the shell lives, the app lives |
| Pi or Beads | `Ctrl-C` | quits the app ([`CHORD_TABLE`](../../src/session/view.rs)) |
| anything, a full-screen child holds the screen | `Ctrl-Q` | the child is killed on the way out and the alt-screen leave is paid exactly once |
| the app has wedged | `kill <pid>`, or `kill -9` | the signal path and the panic path both go through the ledger; the tty comes back cooked either way |

The rule worth memorising: **`Ctrl-Q` always quits; `Ctrl-C` means something else
in Bash.** If you find yourself reaching for `Ctrl-C` to quit, use `Ctrl-Q` and
skip the ambiguity.

If the app is genuinely stuck and you cannot get a keystroke in, `kill -9` is safe
for the terminal. The ledger cannot run on `SIGKILL`, so on that path the terminal
keeps whatever modes were on — that is the one exception, it is why the panic and
SIGTERM paths are the ones with 149 checks
([`spikes/shutdown_e2e.py`](../../spikes/shutdown_e2e.py)), and if you hit it, your
terminal is fixed by `reset` or by a new window.

## Two minutes in: the four things worth trying before anything else

1. **Run something in Bash.** `Tab Tab`, type `ls -l`, `Enter`. The output goes
   into the transcript above the input box. Then `cd /tmp` and run it again: the
   `cd` persisted, because this is a live `bash -i`, not a per-command subprocess
   ([ADR-0001](../adr/0001-bash-terminal-state-pty.md)).
2. **Scroll.** `PageUp` moves the transcript up one page and unpins you from the
   tail. Output keeps arriving; you stop moving. A `▲ 37 new · End for the tail`
   pill appears on the band's bottom row. `End` re-pins. Full story:
   [Living with a transcript](transcript.md).
3. **Copy without the mouse.** `Ctrl-S` arms the copy chord, then `a` copies the
   last answer, `o` copies the last output block, `?` lists the family in a toast.
   `Esc` unarms it. This is the single biggest ergonomic win over a plain CLI —
   [the keymap](keymap.md) has the whole family.
4. **Quit and go find what you said.** After `Ctrl-Q`, the transcript is a file:

   ```sh
   less "$(readlink ~/.local/share/looprs/transcripts/last)"
   ```

   That is the escape hatch that makes a bounded scrollback survivable — the store
   drops rows with a marker, and the marker names the file that kept everything
   ([ADR-0004 R2](../adr/0004-fullscreen-tui.md)). Details:
   [Files, logs and recovery](operator.md).

## When it looks stuck

The four shapes of "stuck", and what each one actually is:

| it looks like | what it is | do |
| --- | --- | --- |
| the row is spinning, nothing new painted | something is genuinely running with a long tail (`cargo build`, a compaction) | read the row's *what*: `working · <bead>`, or the live card in the card band. The [compaction card](../../src/components/compaction.rs) says `⠹ compacting context` — ten to sixty seconds of no transcript movement that is not a hang |
| the row says `cancelling` for a long time | an `Esc` is unwinding and the child is not cooperating | wait for the grace window; the child gets killed and the row says so |
| the row says `child gone` / `not started` | the backend died | the next message restarts it in Pi; in Beads it is a `bd` or `pi` problem — read the log |
| nothing changes at all, row frozen | the app is not drawing because nothing has told it to | any keystroke repaints. If a keystroke does nothing, the run loop is wedged: `Ctrl-Q`, and read [the operator page](operator.md) |

The distinction that saves the most time is between **"no output"** and **"no
process"**. The row reports the process, not the output. An agent thinking for 40
seconds has no output and a live process, and the row says so.

## What this page did not cover, on purpose

* **The board.** If you are here for the beads loop, that is
  [the next page](beads-loop.md).
* **Every knob.** The `LOOPRS_*` variables are in
  [the configuration reference](configuration.md) and nowhere else. This page
  mentions `LOOPRS_MODES` once and does not list its values, because a second
  list of a knob's defaults is a future argument with the first.
* **Where the files are.** That is the [operator page](operator.md), and you want
  it to be one page deep rather than printed on the landing page you skip.

**Something not behaving as this page said?**
[Files, logs and recovery → "Debugging a bad run"](operator.md#debugging-a-bad-run)
is the troubleshooting entry point.
