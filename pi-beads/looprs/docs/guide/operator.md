# Files, logs and recovery

The two questions this page exists for:

* **"I quit and the screen is empty — where did my session go?"**
* **"It misbehaved and I have to report it — what do I send?"**

Everything on this page was resolved the way the code resolves it and then verified
on a real run. Where the code's *behaviour* and a comment disagree, the behaviour
is written here and the comment gets a ticket.

---

## Where things are written

Four sinks, four resolutions, each read **once at startup**. The ladder is in the
order the code tries, not the order you might expect.

### 1. The journal — the transcript, as it happens

```text
$LOOPRS_TRANSCRIPT_DIR
  → $XDG_DATA_HOME/looprs/transcripts/
  → ~/.local/share/looprs/transcripts/          (the default)

session-20261009T034008Z-84367-Beeds.txt    this run, this mode
session-20261009T034004Z-84354-Pi.txt       …and this one
last          → …-Beeds.txt                 the most recently written, any mode
last-Beeds    → …-Beeds.txt                 one per mode, for when you know
last-Pi       → …-Pi.txt
```

Verified on a real run:

```console
$ ls -l ~/.local/share/looprs/transcripts/ | head
total 1387200
lrwxr-xr-x  …  last        -> …/session-20261009T034008Z-84367-Beeds.txt
lrwxr-xr-x  …  last-Bash  -> …/session-20261009T034004Z-84354-Bash.txt
lrwxr-xr-x  …  last-Beeds -> …/session-20261009T034008Z-84367-Beeds.txt
lrwxr-xr-x  …  last-Pi    -> …/session-20261009T034004Z-84354-Pi.txt
-rw-------   …  session-20261007T104926Z-4473-Beeds.txt
-rw-------   …  session-20261007T105039Z-4473-Pi.txt
-rw-------   …  session-20261007T105042Z-4473-Bash.txt
```

* **Modes:** files `0600`, directory `0700`. Mode bits are the whole protection.
* **One file per run per mode that actually ran**, named so they sort
  chronologically: `session-<UTC timestamp>Z-<pid>-<Mode>.txt`.
* **The mode name is `TerminalType::label()`** — capitalised `Beeds`, `Pi`, `Bash`.
  (`journal.rs`'s own header shows it lowercase; the file on disk is the truth, and
  the comment has a ticket.)
* **Written as it finalises, not at exit.** Each entry is appended and flushed on a
  writer task while the session runs. That is what makes it survive `kill -9`, an
  OOM kill, a panic inside the draw, and a laptop out of power. A journal written
  on the exit path is a journal that does not exist after every one of those
  ([ADR-0004 R2](../adr/0004-fullscreen-tui.md)).
* **Plain text, transcript order, no timestamps in the body** — deliberately byte
  for byte what "select everything and copy" would have been. The no-timestamps
  rule is the price of that property and it was a decided trade, not an oversight.

**This file is for reading. It is not the record.** `bd` is the system of record
for work. The journal has no schema, no version, no stability contract; nothing in
looprs reads it back, and a tool built to parse it will be broken by the next
release. If a fact matters, it belongs in `bd`.

**Nothing rotates it.** One file per run per mode until you delete them. Growth is
roughly the transcript size of the run. The off switch is `LOOPRS_TRANSCRIPT=off`
([reference](configuration.md#transcript-journal-and-logging)), and the reason you
might want it is stated in the privacy section below.

### 2. The dump — the transcript you asked for, now

`Ctrl-S t` writes the whole settled transcript to a timestamped file:

```text
$LOOPRS_TRANSCRIPT_DIR
  → $XDG_CACHE_HOME/looprs/
  → ~/.cache/looprs/looprs-<mode>-<hhmmss>.txt
```

Two behaviours worth knowing:

* the **toast folds a leading `$HOME` to `~`** so the file's *name* survives the
  row. That is not cosmetic: macOS's `$TMPDIR` is
  `/var/folders/17/…/T/` — 49 columns of directory before the name — and measured
  through tmux at 100 columns the toast came out cut mid-path and named nothing.
  The dump's default is a cache path for exactly this reason; the absolute path is
  still what gets written and is in the startup log;
* the **live tail is not in the dump**. Neither is it reachable by a drag selection:
  it has no final form. The settled transcript is the thing worth keeping.

Turned off with `LOOPRS_TRANSCRIPT_DUMP=off`.

### 3. The log

```text
$TMPDIR/looprs.log          i.e. std::env::temp_dir()/looprs.log
```

On macOS that is `/var/folders/17/…/T/looprs.log`, **not** `~/tmp` and **not** the
directory you launched from:

```console
$ ls -l "$TMPDIR/looprs.log"
-rw-r--r--  1  …  /var/folders/17/zpwmvyzd1bdc4lkjs6x0_r6c0000gn/T//looprs.log
```

This is the behaviour, not the wish. The destination is hard-coded, there is no
rotation, and the default filter is `debug` rather than `warn` — three facts about
`init_logging` that are the subject of `looprs-00u.13` ("Logging: hard-coded /tmp
destination, no rotation, debug-by-default"). Until that lands:

```sh
# the two things you actually need to know
echo "$TMPDIR/looprs.log"           # where it is
RUST_LOG=info ./target/debug/looprs # how to make it shorter
```

Log lines are plain (`with_ansi(false)`) and carry the `tracing` module path, which
is what makes the grep index below work.

### 4. The `bd` board itself

Not written by looprs. The loop writes through `bd` (claim, close) and the band
reads through `bd --readonly`. Where *that* database lives is `bd`'s business; see
`.beads/` in the workspace and the beads docs. Do not treat the journal as a
substitute for it.

## Recovery recipes

### "I quit too fast / the window closed — read the session"

```sh
less "$(readlink ~/.local/share/looprs/transcripts/last)"
```

### "I want the beads one, not the chat one"

```sh
less "$(readlink ~/.local/share/looprs/transcripts/last-Beeds)"
# and while a run is live, sorted by recency:
ls -lt ~/.local/share/looprs/transcripts/ | head
```

### "The app crashed — is anything there?"

Yes, if it got as far as finalising a single entry. The journal flushes per entry,
so a run that died 20 seconds in has 20 seconds in it:

```sh
ls -lt ~/.local/share/looprs/transcripts/*.txt | head -5
tail -100 "$(readlink ~/.local/share/looprs/transcripts/last)"
```

If the file is empty or absent, the session never finalised an entry — go to the
log, which starts writing before the first frame does.

### "I need the last answer in the clipboard and I am not touching the mouse"

`Ctrl-S a`. In Bash this refuses and tells you so — use `Ctrl-S o` for the last
sealed command block. `Ctrl-S t` if you need the whole thing.
[The keymap](keymap.md#the-copy-family-ctrl-s).

### "I need to see what a dead Beads pass actually printed"

The transcript of that mode, which is the `last-Beeds` file above. If the pass
died mid-tool-card, the card gets sealed as `Aborted` on the way out rather than
leaving an open card that stalls the flush — so the tail you read is complete even
when the process was not
([`SessionView::seal`](../../src/session/view.rs)).

### "Did the board actually change, or did the UI lie to me?"

```sh
grep 'board poll' "$TMPDIR/looprs.log" | tail -20
bd show <your-ticket-id>
```

The log distinguishes a quiet journal from a full read:

```text
2026-10-09T03:40:09Z DEBUG looprs::services::board_poller: board poll: journal quiet at seq 63 (limit 1), no board read this tick
2026-10-09T03:40:20Z DEBUG looprs::services::board_poller: board poll: full read (Sweep), watermark now 63
```

If the band says `stale`, the *rows* are the last good read and the footer's head
names the reason the latest read failed. Nothing needs restarting: the next
successful read repaints live and the `stale` marker goes away by itself.
[docs/kanban.md → reading the footer](../kanban.md#reading-the-footer).

## Debugging a bad run

Work outside-in: **row → transcript → log → spikes.**

**1. Read the status row.** It answers "is there a process, what is it doing, did
anything fail" and the answer is a small closed set. `paused · ✗ …` means `bd` said
no; `child gone` means a backend died; a spinner with no output means something is
running with a long tail.
[Verb table](beads-loop.md#telling-a-finished-pass-from-a-wedged-one).

**2. Read that mode's transcript**, not the one on screen. Each mode has its own
store and its own file; a mode you `Tab`bed away from kept everything.

**3. Then the log.** The greps below are the diagnostic index. They are the same
greps the ADRs and spike READMEs name, collected in one place because finding them
is the annoying part:

| symptom | grep | what it answers |
| --- | --- | --- |
| what is configured | `grep -E 'kanban board\|clipboard:\|transcript dump:\|notifications' "$TMPDIR/looprs.log"` | every knob's resolved choice, announced once at startup |
| the board is wrong / stale | `grep 'board poll' "$TMPDIR/looprs.log"` | quiet journal vs full read vs sweep |
| the change detector broke | `grep 'change detector' "$TMPDIR/looprs.log"` | the break and the recovery, each logged **once**, not every tick |
| the wheel/trackpad ate my scroll | `grep 'wheel gesture closed' "$TMPDIR/looprs.log"` | the throttle's gesture boundaries |
| a knob was ignored | `grep -E 'is not a number\|clamped to\|falling back' "$TMPDIR/looprs.log"` | every loud fallback (there are no quiet ones) |
| something refused to copy | `grep -iE 'clipboard\|copied\|Nothing copied' "$TMPDIR/looprs.log"` | the transport that was tried and the count that landed |
| the terminal did not come back | `grep -E 'restore\|ledger\|screen debt' "$TMPDIR/looprs.log"` | which modes were held and which were handed back |
| the app panicked | `grep -A20 panic "$TMPDIR/looprs.log"` | the panic hook ran, the ledger ran, the tty came back |

**4. Reproduce it with a spike.** Every e2e spike can run against an arbitrary
binary, which is how you find out whether a behaviour is new:

```sh
# current build
python3 spikes/status_e2e.py
# the pre-change build, same script
git worktree add --detach /tmp/looprs-ctl HEAD~20
(cd /tmp/looprs-ctl/pi-beads/looprs && cargo build --target-dir /tmp/ctl-target)
LOOPRS_BIN=/tmp/ctl-target/debug/looprs python3 spikes/status_e2e.py --control
```

[spikes/README.md](../../spikes/README.md) is the index of what each spike measures.

### What to attach when you file a bug

In order of usefulness:

1. **the transcript file** — `~/.local/share/looprs/transcripts/session-…-<mode>.txt`
   (or the `last` symlink copied out). This is the single most useful artefact;
2. **the log** — `$TMPDIR/looprs.log`. Trim it if it is huge: the startup lines
   (first ~50) plus the window around the failure;
3. **the exact knob set** — `env | grep '^LOOPRS_' | sort`, plus `RUST_LOG`;
4. **the terminal**: `$TERM`, the terminal app and version, whether you were in tmux
   or SSH, and the window size;
5. **what you expected instead**, and — if you have one — the control-run result
   showing whether the previous build did the same thing.

Trim the transcript before attaching it in a public place. See below.

## Privacy, plainly

**These files contain everything the agent saw.** That includes file contents, tool
output, command lines, and any secret that passed through a terminal — a token in a
`curl` invocation, a `.env` value a tool echoed, a customer record a query printed.

What protects them:

* journal files `0600`, directory `0700` — **mode bits are the whole thing**;
* the journal lives in your own data directory, not a world-readable temp dir;
* the dump path folds `$HOME` to `~` in the *display*, not in what is written, so
  the path in a toast is not leaking anything — the path on disk is the real one;
* the log is written where `temp_dir()` points, which on macOS is per-user but on a
  shared machine may not be. Check `ls -l "$TMPDIR"` if that describes your machine.

What does **not** protect them: encryption, redaction, or any filtering. There is no
secret scrubbing anywhere in the write path, and nothing will catch a token before it
lands in a journal file.

The knobs, in order of how much they cost you:

| want | knob | what you lose |
| --- | --- | --- |
| nothing transcript-shaped on disk | `LOOPRS_TRANSCRIPT=off` | recovery from a crash, the `last` symlink, "what did the loop say at 03:12" |
| no dump files | `LOOPRS_TRANSCRIPT_DUMP=off` | `Ctrl-S t` |
| nothing on disk at all | both of the above, plus `RUST_LOG=off` | every diagnostic. A bug report becomes "it did a thing" |
| keep the journal, somewhere else | `LOOPRS_TRANSCRIPT_DIR=/secure/place` | nothing but your assumptions about the default |

A middle path that works well in practice: keep the journal, and point it at an
encrypted volume (`LOOPRS_TRANSCRIPT_DIR=~/secure/looprs` inside a FileVault /
`encfs` / encrypted home), so the failure-recovery value stays and the at-rest
exposure is the same as the rest of your secrets.

## What looprs does not do

Because "why doesn't it just…" questions deserve an answer before they are asked:

* **It does not resume a session.** Quit, and the process is gone. The transcript is
  a file, not a resumable state. `tmux`/`screen` is the answer if you want the
  process to survive the connection.
* **It does not sync, back up, or rotate anything.** The journal and the log grow
  until you clean them up.
* **It does not scrub secrets.** Read the privacy section again; it is the only
  section here you cannot work around with a knob.
* **It is not a shell wrapper that remembers your commands.** Bash history is bash's
  own, in the shell's `~/.bash_history` — with the caveat that the Bash mode runs
  with an injected `--rcfile` integration, so the history file is bash's business,
  not looprs's.
* **It does not do anything with `git`.** No commit, no push, no worktree
  management. That is your (or the agent's) business, in the Bash mode or outside.

**See also:** [configuration reference](configuration.md) ·
[docs/kanban.md](../kanban.md) · [docs/testing.md](../testing.md) ·
[spikes/README.md](../../spikes/README.md)
