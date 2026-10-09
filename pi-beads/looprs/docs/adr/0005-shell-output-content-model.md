# ADR-0005: What shell output may contain, now that looprs wraps it itself

- **ID:** looprs-pdl.5
- **Status:** Accepted — 2026-10-06
- **Epic:** looprs-pdl (Full-screen TUI: own the screen, the scrollback, the selection, the clipboard)
- **Decides for:** looprs-pdl.6 (the scrollback store), looprs-pdl.7 (bounded scrollback),
  looprs-pdl.8 (mouse scroll), looprs-pdl.9 (drag selection over wrapped lines),
  looprs-pdl.10 (select-to-copy), and Bash mode as it runs today
- **Supersedes:** ADR-0001 rule 5 ("the transcript copy keeps the content and drops the
  presentation") for `MessageKind::Bash`. Rule 1 ("no markdown, no re-wrap") still stands,
  unchanged, and this ADR is written so that it keeps standing.
- **Numbering note:** ADR-0004 is deliberately left unissued here. It belongs to
  looprs-pdl.1 (which screen we take, how the clipboard gets written, what a selection
  copies). Taking the slot for this decision would put the two ADRs in the wrong order.

---

## Decision

**Shell output is not a string. It is a byte stream with a presentation, and the
presentation has to be *resolved* before anything is stored.** Exactly one component does
that — [`utils::shelltext::LineResolver`] — between the pty reader and the transcript. It
owns a cell-addressed line buffer, applies the child's own editing to it as the bytes arrive,
and hands finished lines to the store as:

```text
child bytes ─► LineResolver ─► StyledLine { text: plain content, runs: styles, cells: cells }
                                   │              │                    │
                             what we copy   what we render        what we wrap in
```

Seven rules, each of them in code, each of them tested:

1. **SGR is kept.** `CSI … m` is decoded into a `ratatui::style::Style` and rides on the
   text as styled runs. Colour is the one thing in shell output that is worth the work, and
   the user's own palette paints it because colours stay **indexed**.
2. **`CR`, `BS` and `EL` resolve inside the line.** They are edits to the line the child is
   writing, not line endings and not decoration. `\n` from the child is the only thing that
   ends a line.
3. **Tabs expand** to blanks, to 8-cell stops, in screen columns, never crossing the row.
4. **Wide characters are cells.** A cluster takes the number of cells `unicode_width`
   measures for it — and `unicode_width` is what the renderer measures with, so
   `line.cells == line.text.width()` is an asserted invariant, not a hope.
5. **Everything else the child emits is dropped**: OSC, cursor addressing, scroll regions,
   alt-screen. Dropped, not stored-and-ignored: no `ESC` byte survives resolution.
6. **What is stored is control-free** — no `ESC`, no C0 controls, no `TAB`. That is the
   property the copy path is built on, and it is asserted on every line.
7. **Copy is the content, not the picture.** The paste value is `StyledLine::text` with
   trailing blanks removed. Styles never reach a paste; layout blanks never reach a paste.

**Rule 1 of ADR-0001 is untouched by all of this.** Shell output still never enters
`utils::md`, and nothing in the resolver re-wraps anything: the resolver emits a line only
when the *child* ended one. Where ADR-0001 said "verbatim", this ADR says "**resolved**",
and says out loud what each resolution keeps and what it throws away. "Verbatim" was only ever
coherent while somebody else — the terminal — was doing the resolving.

---

## Why the old promise had to be re-made

ADR-0001 rule 5 was implemented by `utils::render::ControlStripper`: keep `\n` and `\t`,
drop every other C0 control, drop escape sequences whole (statefully, so a sequence split
across two reads dropped as one). Against a transcript that nobody re-flowed, that was
cheap and it kept the noise out.

It is not correct for output *we* lay out. The stripper deleted the mechanism of `\r` while
keeping everything `\r` was painted over. Measured on `tests/fixtures/shell_output/progress-wides.raw`
(a 31-frame bar, each frame started with `\r`, closed with `CR EL done`), the old rule leaves:

```
[------------------------------] 0%[###---------------------------] 10%[######------…
```

**one line of 402 characters**, every frame concatenated. Not noisy — *wrong*, and wrong in
the one place (a line the user was watching change) where it is most visible. The same rule
leaves `\t` in the stored text, where `unicode_width` measures it as **one column** while
the terminal, when that byte goes back out, expands it to the next stop: our grid and the
terminal's cursor disagree from that point on.

Both failures come from the same mistake: treating the presentation as something to remove
rather than something to *apply*.

---

## The four questions, answered

### Q1 — SGR colour: strip, decode, or pass through?

**Decode the subset into `Style`, keep colour, keep it indexed.**

`strip every escape` was the default until now and it is a real loss, not a cosmetic one:
`ls --color`, `grep --color`, `git diff`, pytest, rustc's own diagnostics, and every
`LESS`-style colour scheme in the user's dotfiles put meaning into colour — modified-added-removed,
error-warn-info, file-type. A terminal-less viewer that discards it is telling the user less
than `cat` tells them.

`pass the bytes through` is not available. We own the cell grid: an unhandled `\x1b[31m`
either paints literal garbage into cells we will never reclaim or moves a cursor we are not
tracking. This is the option that is impossible rather than expensive, and it is worth naming
because "just don't touch the bytes, that's what verbatim means" is the intuitive answer and
it is wrong.

`decode the subset` is cheap, closed, and testable as a table. Supported:

| Parameter | Meaning |
|---|---|
| `0`, empty | reset (`\e[m` means SGR 0 — it is what `git` closes every span with) |
| `1 2 3 4 5 6 7 8 9` | bold, dim, italic, underline, slow blink, rapid blink, reverse, hidden, crossed out |
| `21` | underline (xterm's doubled underline is not modelled; underline is the visible reading) |
| `22 23 24 25 27 28 29` | each one turns off the thing it names — `22` turns off **both** bold and dim, and nothing else |
| `30–37`, `90–97` | the 8 + 8 ANSI palette, as `Color::Indexed(0..8)` / `(8..16)` |
| `38;5;n`, `48;5;n` | 256-colour |
| `38;2;r;g;b`, `48;2;r:g;b` | truecolour |
| `39`, `49` | "back to the terminal's default" — recorded as `Color::Reset`, which is not the same fact as "nothing was said" |
| anything else | ignored, **text kept** |

Three things in that table are deliberate and each has a test watching it:

- **Indexed, not named.** `SGR 31` resolves to `Color::Indexed(1)`, never to `Color::Red`,
  so the user's terminal theme paints their red. Mapping to named colours would silently
  override the palette the user chose.
- **`58`/`59` (underline colour) are consumed, not applied.** They must be *eaten*: if the
  `38`-shaped tail is read but the group is not consumed, the `;5;196` of an underline colour
  comes round the loop again and `5` reads as "slow blink". That is the exact failure this
  branch exists to prevent, and a test asserts a `58;5;196m` string comes out with no style
  at all.
- **Colon sub-parameters kill the sequence.** `SGR …:…` is outside what we interpret, and
  applying the digits in front of it as if the tail were a normal `;` list is a guess at a
  sequence we just admitted we cannot read. The sequence is dropped; the text survives.
  Colour is where a guess turns into noise.

**Cost, honestly:** ~95 lines of closed decoder (`apply_sgr` plus its extended-colour tail), no
new dependency (`ratatui::style` is already here). It is not "full ANSI colour in shell output" — it is the SGR subset, which is
what `git`/`ls`/`grep`/`pytest`/`cargo` use; the 16/256/truecolour paths are all supported
so nothing a modern theme emits falls off. What is *not* attempted: `SGR` colours of
underlines, overline, and the double-underline variants, all of which are ignored rather than
approximated.

### Q2 — Cursor movement inside ordinary output: is `\r` end-of-line, and where does `\b` land?

**`\r` is not end-of-line. It is "return to the start of the current row" and then
overwrite in place. `\b` moves one *cell* left and the next write overwrites what is there.
`EL` (`\x1b[K`) is honoured, because it is the child's own way of saying "the tail I did not
overwrite should not be there".**

| Input | Resolves to | Rule |
|---|---|---|
| `abc\n` | `abc` | `\n` is the only line ending |
| `abc\rXY` | `XYc` | `\r` returns to the row start; the tail the child did not erase stays |
| `AAAA\rBB` | `BBAA` | same: we do not tidy up behind a program that did not ask |
| `AAAA\r\x1b[KBB` | `BB` | `EL 0` — cursor → end of line |
| `AA\x1b[1Kx` | `"  x"` | `EL 1` — start → cursor inclusive, then the write at the cursor |
| `\x1b[2K` | the line cleared to blanks, **not ended** | `EL 2` clears; the cursor stays |
| `abc\bX` | `abX` | `\b` = one cell left, and the `X` replaces the `c` |
| `abc\b\b\b\b\bX` | `Xbc` | `\b` clamps at the left edge; it never walks into the previous line |
| `0123456789ab\rX` at wrap 10 | `0123456789Xb` | with a wrap width, `\r` means *this row*, not the head of the whole logical line |
| `0123456789\bX` at wrap 10 | `0123456789X` | at a row's left edge `\b` has nowhere to go, exactly as at column 0 |

The wrap-width row is the one that costs something to explain, so: the resolver is told the
width **the pty was given** — the same value `App` forwards on resize — and every horizontal
motion is computed in *screen* columns within it. That is not a flourish. It is what makes a
`curl`-shaped bar that touched the right margin land on the row the child believed it was on,
instead of overwriting the head of a line two rows up. It is also why the store can hold
*logical* lines and be re-wrapped at will by looprs-pdl.6: a terminal that re-wraps its own
buffer on resize applies its overwrites to cells before the re-wrap, and we apply ours to the
logical line before the re-wrap, and those come out at the same place.

**Why `\r` is not end-of-line.** Because a `\r` has never been an end-of-line; the pair
`\r\n` is, and a bare `\r` in shell output means "put the cursor back and paint over it" —
which is what a progress bar, a readline redraw, a `pv` meter and a `git clone` percentage
all do. Treating `\r` as end-of-line means a repaint becomes a new line, so a single line
the user watched change becomes 31 lines of history that scroll past them. (That, plus
rule 5's concatenation, is the thing the last section measured.)

**What we lose:** a program that repaints by *moving up a line* — `CSI A` and friends — loses
the addressing and its text lands appended to the line we are on. We do not reconstruct a
screen, because a transcript is a list of lines and a screen operation has no line-level
meaning. The text is never lost; the layout of that repaint is. This costs little in practice
because full-screen children never reach the resolver at all: ADR-0001's screen-buffer path
tees their bytes straight to the terminal, so the sequences we decline to interpret are mostly
ones a line-mode program has no reason to emit.

`ICH` (insert blanks) and `DCH` (delete cells) are dropped for the same reason and a second:
they are cell surgery in the middle of a line, and honouring them correctly across wide-cluster
boundaries is real work for output that cannot reach us from anywhere but a screen we are not
painting.

### Q3 — Tabs: expand, to what, and whose width decides?

**Expand, to 8-cell stops, in screen columns, and store blanks.**

- Stop = 8 cells, the VT100 default, and the assumption every program that emits a tab made.
- The stop is computed in **screen** columns (ADR-0005's wrap width), and a tab never
  crosses the end of its row: at a 5-cell row with the cursor at 3, a tab buys 2 and stops.
  A tab that wrapped the row would be inventing layout, which is exactly what we are not
  allowed to do to the child's line.
- What is stored is blanks, not the `\t`.

**Why not keep the tab and expand at render?** Because we would then have to be *equally*
right about tab stops in four places — the live preview, the scrollback store, the wrap, and
the selection/copy mapping — and today we are measurably wrong in the one place that cannot
afford to be: `unicode_width` measures `\t` as **one column**, so the layout budget says one
cell while the terminal, seeing that byte on the way out, moves to the next stop. Every cell
after it in the row belongs to a different column than we think. Expanding in one place, once,
at the boundary where we still know the child's column, is the only version that cannot drift.

**What we lose, stated in the open:** a tab-indented file `cat`ted in Bash mode pastes with
spaces, not tabs. The content is intact; the *tab characters* are not, and a paste into an
editor will not reconstitute them. Accepted for the first cut because the alignment the user
saw is preserved exactly and the alternative is a per-run tab character with a cell width
attached to it in the mapping (which is what a real terminal emulator does — xterm copies a
tab as a tab). **Follow-up for looprs-pdl.9/10:** if that costs nothing once runs carry
cluster widths, keep the ` `\ but carry the original byte in the run and copy that instead.
Note that this is *only* shell output; a file the user opens elsewhere, and their own typed
input, are untouched.

### Q4 — Wide characters, combining marks, and the two mappings

**The store keeps cells and characters, and they are the same bytes.** A *cluster* — a base
plus whatever joins it — is the unit. It is laid out in cells, `unicode_width`-measured, and
its characters live, intact, in the line's `text`:

- **`line.cells == line.text.width()`** — asserted in `StyledLine::compose`, on every finished
  line and on every live preview line. Both sides get that number from the same crate, and the
  same one the renderer uses, so a wrap counted in cells and a wrap counted on text cannot
  drift apart. This is the invariant the whole cell/character mapping stands on, and
  `the_cells_a_line_takes_are_the_width_the_renderer_measures` runs it over a corpus.
- **A cluster occupies `cluster_cells(text) = max(1, text.width())` cells** — deliberately
  *not* clamped at two. An exotic joined sequence may measure more, and clamping it would put
  the cell grid and the wrap out of step, which is the one disagreement that corrupts layout
  rather than merely looking odd.
- **A joiner extends the cluster to its left** instead of taking a cell: combining marks,
  variation selectors, flag tag characters, and `ZWJ`. The emoji **skin-tone modifiers**
  (`U+1F3FB..U+1F3FF`) are the exception worth writing down: `unicode-width` calls a bare
  one two cells wide, and it is — but after 👩 it *joins* it, and the joined `👩🏽` measures
  two. Laying that out as a separate two-cell cluster would put us a whole glyph out of step.
- **`ZWJ` additionally pulls the next base into the same cluster.** `👩‍👩‍👦` is one
  cluster of two cells, not three emoji of six, because that is what the renderer measures.
  When the measure changes under a joined codepoint the cluster's continuation cells are
  resized with it (`☺` one cell, `☺️` two).
- **Half-overwriting a wide cluster blanks the whole thing.** Writing into a cluster's second
  cell replaces the entire cluster and turns its freed cells into blanks — lead *and* tail.
  Anything else leaves a continuation with no lead in front of it, which repaints as a broken
  glyph for the rest of the line's life.
- **A joiner with nothing to join is the only content a resolved line can lose.** A bare
  combining mark at column 0 cannot take a cell without breaking the invariant above, and a
  cell of loose accent is not information. It is dropped and logged at `trace`.
- **Cluster splits across a read boundary are fine and tested.** Resolution is per-codepoint
  with the join rule attached, so the base arriving in one chunk and its mark in the next
  lands them in the same cell — `cutting_the_stream_one_character_at_a_time_changes_nothing`
  runs every corpus string and every fixture that way.

**The three projections, and what each one carries.** The ticket asked for the store, the
render and the copy to be stated separately, and allowed the seen value and the pasted value to
differ. They mostly agree, and where they differ it is by exactly one field:

| Path | Value | Contains | Explicitly does **not** contain |
|---|---|---|---|
| **Store** (`Entry`) | `text: String` + `styles: Vec<StyleRun>` (byte ranges into `text`) | the child's resolved characters, its colours, and `cells` | escape bytes, C0 controls, tabs, layout blanks of any kind it did not write |
| **Render** | `Line`/`Span` from `text` clipped by `styles` | the picture: content + colour + the blanks that made alignment | — (styles clip; the gap between runs is raw text so "no style said anything" means default, not "print nothing") |
| **Copy** (pdl.10) | `StyledLine::copy_text()` = `text` with trailing blanks trimmed | exactly what the characters were | styles (never), layout blanks (trimmed), the child's escape bytes (already gone) |

Consequences worth spelling out for the tickets downstream:

- **Selection (pdl.9) can snap to whole characters** because a line's cell count and its
  character content are two views of the same string, tied by the asserted invariant: a cell
  range maps to a character range by walking clusters with `unicode_width`, and there is no
  second table to keep in sync — and nothing to *guess*.
- **Copy is safe because of rule 6, not because of a filter at copy time.** The store cannot
  contain a control character. That is checked (`is_control_free`), and it means pdl.10 has
  no sanitizer to forget.
- **Pasting a resolved line into an editor yields plain text with no ANSI noise**, and — this
  is the "different answer" the ticket allowed — it yields no *colour*, deliberately. Colour
  is presentation; the terminal the user pastes into has its own.
- **Re-wrap on resize (pdl.6) is a pure function of `cells`** — no re-parsing, and no
  possibility that the second wrap disagrees with the first because it re-interpreted a byte.

---

## What we lose, priced

Named in the open so it is a decision rather than a surprise left inside a data structure:

| Loss | Size | Why accepted now | Who pays it off |
|---|---|---|---|
| Tab characters in shell output become blanks | one field in the run mapping | the alternative is a stored byte our own width function measures wrong | pdl.9/10, if runs carry cluster widths |
| Repaints that address the screen (`CUU`/`CUD`/`ICH`/`DCH`/`ED`) are dropped; text kept, layout approximate | small | a transcript is a list of lines; full-screen children are teed and never reach us | nobody, deliberately |
| Doubled underline (`21`), framed (`53`), underline colour (`58`/`59`) | ignored | not modelled by ratatui's `Modifier` set; ignoring beats faking | — |
| A lone combining mark with no base is dropped | ~never happens | it cannot take a cell without breaking the width invariant | — |
| The *animation* of a progress bar | gone by design | a transcript records the last state of a line, not a movie; the frames were never content | — |
| The user's `TERM`-specific palette is whatever their terminal theme says | n/a | indexed colour is the only promise worth making | — |

**This is not "strip, and record the loss."** It is: colour kept, in-line editing applied,
tabs and clusters laid out in cells, and the only losses listed above are ones that come from
not reconstructing a screen we do not own.

---

## Where it lives

| Piece | Where | Note |
|---|---|---|
| The decision, executable | `src/utils/shelltext.rs` | `LineResolver`, `StyledLine`, `StyleRun`, `spanned`, `is_control_free` |
| SGR table | `LineResolver::apply_sgr` / `extended_color` / `params` | closed set, table-tested |
| Cluster rules | `is_joiner`, `cluster_cells`, `put_char`, `place_cluster`, `extend_cluster` | the cell/character half of the mapping |
| The store | `state::transcript::{Entry.styles, Transcript::push_shell_lines}` | text and styles appended together, ranges re-based onto the entry |
| Render | `components::line_render::Flusher::drain_raw`, `spanned` | clips runs to the line, fills gaps with raw (`line_render` is what this module was called `scrollback` until looprs-di9 gave the word to the store alone) |
| Live tail | `session::view::SessionView::preview` | the resolver's open line, styled |
| Stream state | `session::view::SessionView::shell` | one resolver per Bash view; `flush_shell_pending` on every path that ends the stream |
| Wrap width | `App::width` → `push_bash(chunk, width)`, same value as `forward_resize` | **must** stay equal to the pty width, or `\r` means the wrong row |
| Retired | `utils::render::ControlStripper` (deleted here) | its "no escape bytes reach the store" coverage moved into `CORPUS` |

`utils::render` is now just the spinner. The stripper is deleted rather than deprecated because
it was not a worse version of something better — it was *wrong* for this, per the 402-character
line above, and leaving two answers to "how does shell output get cleaned" in the tree is worse
than one of them being gone.

---

## Evidence

**Unit: 53 tests in `utils::shelltext`**, one per rule, plus:

- `cutting_the_stream_one_character_at_a_time_changes_nothing` — 22 corpus strings through the
  same resolver twice, whole and one character at a time. This test found a real bug: an OSC
  terminator (`ESC \`) landing on a chunk boundary never ended the OSC and swallowed the rest
  of the stream silently. That is why `State::OscEsc` exists.
- `a_curl_style_bar_becomes_its_last_frame_instead_of_a_hundred_frames` — 11 frames + `EL` →
  one line, `"done"`.
- `the_cells_a_line_takes_are_the_width_the_renderer_measures` — the invariant, over the corpus.
- Wide/cluster set: VS16 widening, skin-tone joining, flag tag sequences, `漢字` measuring 4,
  decomposed `é` taking no extra cell, `漢\bX` → `" X"`, `漢\rX` → `"X "`, `漢字\rab` → `"ab字"`.
- SGR set: the 9 attributes and their off-switches, `38;5;n`/`38;2;r;g;b`, bright palette
  indexed 8–15, `58` not becoming `38`, colon sub-parameters dropped, unknown codes keeping
  their text, style carrying across `\n`.

**Fixtures, captured from real programs** (`tests/fixtures/shell_output/`, committed raw so
they can be re-captured and diffed):

| Fixture | From | What it proves |
|---|---|---|
| `git-diff.raw` | `git -c color.ui=always diff` | the `\e[32m+\e[m\e[32m+text\e[m` shape keeps its green and merges to one run per span; old rule left the same content with no colour at all |
| `ls-color.raw` | `ls --color=always` | directories keep `\e[34m…\e[39;49m\e[0m` — blue survives as `Color::Indexed(4)` |
| `progress-wides.raw` | a curl-shaped 31-frame bar, `EL`-closed, plus tabs, CJK, ZWJ family, VS16, decomposed e-acute | old rule: the bar line is **402 chars** of concatenated frames. New: `"done"`. Tabs land on stops 8/16. Wide and combined text measures and copies exactly |

**Spike suite, against the new binary** (`target/debug/looprs`), per the acceptance criteria:

| Spike | Result |
|---|---|
| `bash_e2e.py` | **20/20** |
| `viewport_e2e.py` | **16/16** |
| `cancel_e2e.py` | **19/19** |
| `status_e2e.py` | **20/20** |
| `shutdown_e2e.py` | **25/26** (`spikes/results/shutdown-e2e-at-pdl5.log`) — `exactly one closing newline after the erase` fails **identically on the pre-change control** (`HEAD` built to its own target dir: 25/26, same check). Pre-existing, unrelated to shell output; not chased here and not hidden |
| `flash_e2e.py` | **0/3** (`spikes/results/flash-e2e-at-pdl5.log`; worst erase→content hole 8.80–9.26 ms) — the pre-change control fails the same way at **9.44 ms**. Pre-existing, same magnitude, not introduced by this ticket. Recorded rather than fixed: it is a frame-scheduling problem in the inline viewport's erase/paint split, which looprs-pdl.4 replaces wholesale |

Both pre-existing failures are committed as evidence: `spikes/results/flash-e2e-at-pdl5.log`
and `spikes/results/shutdown-e2e-at-pdl5.log`, taken against this binary, alongside the
control runs quoted above (`HEAD` → its own target dir, `LOOPRS_BIN=… python3 spikes/…`).
Two spikes that used to pass do not pass at this revision in this environment, and neither of
them passes without this ticket's code — that is the state, written down where the next person
will look rather than left for them to rediscover mid-migration.
| `./scripts/check.sh` | clean (fmt, `clippy --all-targets -D warnings`, 410 tests) |

---

## Alternatives considered

1. **Keep stripping (ADR-0001 rule 5 as implemented).** Rejected: it doesn't resolve, so a
   `\r` repaint concatenates (402 chars measured) and a stored tab measures one cell while the
   terminal expands it — two layout errors, both silent, both in the output the user is watching.
2. **Pull in a terminal-emulator crate (vte, alacritty_types, a full `ansi-to-tokio` style
   library).** Rejected for this: what we need is *line*-level, not screen-level, and the
   screen-level part is already handled by the tee path (ADR-0001 Amendment 3). A screen
   emulator would give us an answer to questions we decided not to ask (scroll regions, cursor
   addressing, alt-screen), at a dependency and a mental model far bigger than the SGR +
   in-line-edit subset. ~95 lines with a closed table and a 22-string corpus is cheaper and reviewable.
   If a later ticket *does* want a screen model, this file is the place it forks from.
3. **Store the child's original bytes and resolve at render.** Rejected: every render path
   (live preview, scrollback, wrap, selection, copy) would need the same stateful resolver,
   each of them needing the stream's history — and copy would need a second decoder to get the
   noise back out. Resolving once at the boundary makes the store plain and the styles extra.
4. **Expand tabs at render instead of at the boundary.** Rejected in Q3: it moves the same
   arithmetic into four places that must agree, and the measurement is already wrong for `\t`.
5. **Force SGR reset at every line end.** Rejected: a terminal's SGR state does not reset at
   a line break, and neither does ours. Colours a program left on stay on that program's next
   line, which is what the user sees in a real terminal — and per-entry `styles` mean it can't
   leak into a different kind of entry anyway.
6. **`Cell` = `char` rather than cluster.** Rejected: a cluster is the unit a terminal lays
   out, and `unicode_width` is context-sensitive across a cluster (`ZWJ`, `VS16`, skin tones)
   but additive across cluster boundaries — which is exactly what makes the invariant in Q4
   hold. Per-char widths would measure a ZWJ family as 6 cells and disagree with the renderer.

---

## Follow-ups

- **pdl.6 (store).** `StyledLine` is the store's line type: `text` + `styles` + `cells`.
  Re-wrap is a pure function of `cells`; nothing re-parses.
- **pdl.6/7.** `Entry::styles` is per-entry and travels with eviction; the styles of an
  evicted entry go with it, so there is no separate bookkeeping to forget. The view's buffer
  cap counts the transcript *plus* `LineResolver::pending_len()` — the line still being
  resolved is invisible to the store, and it is precisely the thing a child that never ends a
  line can grow without bound.
- **pdl.9/10 (selection/copy).** Cell-range → char-range by walking clusters with
  `unicode_width`. `copy_text()` is the trim rule; keep it in one place. If runs carry cluster
  widths cheaply, revisit the tab-preservation loss in Q3.
- **Both `App::width` and the pty resize must keep feeding the resolver the same number.**
  If they diverge, `\r` resolves against a row the child never had. There is no runtime check
  for that yet — it is a candidate for a `debug_assert` in whoever next owns that pair.
- **flash / shutdown spike failures** recorded above as pre-existing; both die with the inline
  viewport in pdl.4. **Outcome after `pdl.4`:** both are gone. `shutdown_e2e.py` passes
  **153/153** (`spikes/results/shutdown-e2e.log`) with its erase-anchored checks replaced by
  alternate-screen shape checks, and `flash_e2e.py` passes **4/4**
  (`spikes/results/flash-e2e.log`) with its premise rewritten — zero partial erases on the
  wire, with the pre-pdl.4 binary now failing that check at **26 erases / 1.83–3.58 ms**
  (`spikes/results/flash-e2e-pdl4-control.log`). See the spike table in `docs/testing.md`.
