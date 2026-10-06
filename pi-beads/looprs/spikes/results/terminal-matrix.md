# The terminal matrix — looprs-pdl.2

Per-terminal measurements behind the seven claims that looprs-pdl.8/.9/.10/.12 and
ADR-0004 (looprs-pdl.1) are built on. Lifted straight into the ADR is the intent:
every cell here is a number or a yes/no that came off a wire, and every cell that
could not be measured says what would measure it.

Machine used: macOS 26.6.2, `crossterm 0.29.0`, vim 9.1 (2024-01-02, compiled
2026-07-31), docker `sshd` in `debian:bookworm-slim`, python 3.13.5.

Logs this table is read from:

| log | what it is |
| --- | --- |
| `mouse-clipboard-e2e.log` | the bare-pty groups (inject, burst, shift, modes, vim, clipboard) |
| `mouse-clipboard-e2e-control.log` | the control run: decode-off, malformed SGR, +5 differential, SIGKILL residue, OSC 51 |
| `mouse-clipboard-ssh.log` | the SSH leg: mouse injection and OSC 52 through a real `sshd`, with and without a remote pty |
| `emulator-leg-apple-terminal.log` | the emulator leg in Apple Terminal 470.2 |
| `emulator-leg-wezterm-ssh.log` | the emulator leg in WezTerm `20240203-110809-5046fc22`, including the OSC 52 copy issued on the remote host and landed in the local clipboard |

## The seven claims

| # | claim | measured answer | where |
| --- | --- | --- | --- |
| 1 | **A selection can be driven from outside.** SGR 1006 reports written into a pty arrive at the app inside it as the event kinds and grid coordinates expected. | **Yes.** 13/13 report types decoded 1 event per report, kinds and buttons all correct. The wire is 1-based and crossterm reports 0-based cells: the offset is a constant **(1, 1)** on both axes, with no variance across the 13 reports. A **+5 differential** run moved every reported coordinate by exactly +5. | `inject` |
| 2a | **The clipboard bytes we emit are the bytes a terminal can use.** | **Yes.** `crossterm`'s own writer emits `ESC ] 52 ; c ; <base64> ESC \` (terminator `1b 5c`, ST — not BEL), and the base64 body decodes to the exact payload. A copy written in 7 chunks reassembles to one sequence. | `clipboard` |
| 2b | **The clipboard round trip is observable and honest.** | **Yes where OSC 52 exists**: WezTerm landed 64 B → 1 MiB byte-exact, `latency 0.01 s`, and `pbpaste`/`wl-paste`/`xclip` read-back was verified honest on the host. **No on Apple Terminal**: nothing landed at any size. | `--in-terminal` legs |
| 3 | **OSC 52 limits, per terminal.** | **WezTerm: no cap found up to 1 MiB** (every size landed byte-exact, so no silent clamp observed in the range tested). **Apple Terminal: the cap is 0 bytes** — a 64 B copy did not land in 6.01 s. Latency in the passing range was 0.01 s = **no permission prompt** for writes; the read-back query got no answer in either terminal. | ladder in the legs |
| 4a | **How much the mouse path can carry.** | 128 SGR scroll reports in one 1,536-byte write: **128/128 decoded, 0.301 ms span, worst inter-event gap 0.037 ms, ~4.3×10⁵ events/s**, arriving at the harness in 75 pty chunks. Nothing merged, nothing dropped, at every size tried (10/50/128). | `burst` |
| 4b | **What a real trackpad flick actually sends.** | **NOT MEASURED — no finger on this path.** The row above is the ceiling a flick has to fit under, not the flick. | `--in-terminal --record-flick` (needs a hand) |
| 5a | **The shift bit survives to the app.** | **Yes.** Wire `0x04` → crossterm `SHIFT (0b001)`, wire `0x10` → `CONTROL (0b010)`, wire `0x08` → `ALT (0b100)`. Note the ordering is **not** a shift of the wire bits — alt and ctrl swap places on the way in, so a handler that guessed would read a ctrl-drag as an alt-drag. | `inject`, `shift` |
| 5b | **Does shift-drag still reach the terminal's native selection while we hold 1000+1002+1006?** | **NOT MEASURED — needs eyes.** Nothing comes back up a pty to say whether the window painted a selection band. | `--in-terminal --record-flick` (asks for two shift-drags and a `y`/`n`) |
| 6 | **Mode set/restore symmetry.** | **Yes** for the real binary with `LOOPRS_MODES=all`: all six modes taken, each left **exactly once** (except `cursor_hidden`, see below), nothing still on at the end, tty cooked again, cursor visible, and `lflags` after exit identical to the clean baseline (1483). **6** leave sequences written after the quit key. | `modes` |
| 6a | (the one exemption, measured rather than waived) | `cursor_hidden`: **on 1×, cleared 3×** over one run — a frame that ends with no cursor writes `?25h` at the end of every draw, so the hand-back is the last of several clears, not the only one. Same exemption `shutdown_e2e`'s ledger check makes. | `modes` |
| 7 | **A full-screen child's nested mouse modes.** | vim 9.1, `-u NONE` (vim's own mouse default): vim never touches our `?1000/?1002/?1006` — they are **still ours and still on** at the end; vim set and left its own `?1049` and bracketed paste. vim with **`:set mouse=a`**: vim **switches all three of our mouse modes off on exit** — after a mouse-tracking vim, the app's mouse input is dead unless the app re-asserts. **SIGKILLed** vim: leaves `alt_screen`, `bracketed_paste` and all three mouse modes switched on, nothing restored. | `vim` |
| 7b | **What the user sees while vim holds our alternate screen.** | **NOT MEASURED — the pty has no window.** | run vim inside the real-terminal leg |

## Per terminal

| terminal | DA1 (control on the query path) | DECRQM 1000/1002/1003/1006 | DECRQM 1049 / 2004 | OSC 52 write | largest copy that landed whole | OSC 52 write latency (prompt proxy) | OSC 52 read-back | mouse reports driven from outside |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| **Apple Terminal 470.2** (macOS 26.6.2) | answered `1b5b3f313b3263` → the wire is live | **no answer at all**, all four | **no answer at all** / **no answer at all** | **does not reach the clipboard** (64 B, 6.01 s) | **0 B** | n/a (never landed) | **no** (0 bytes back) | **cannot be driven at all** in this terminal: it answers nothing about mouse modes and implements no xterm mouse reporting; selection here is the terminal's own, always |
| **WezTerm** `20240203-110809-5046fc22` | answered (`?65;4;6;18;22c`) | **all four answered: 2 (reset = present, off)** | `0` (answered-but-unavailable) / **2** | **byte-exact at every size** | **1 MiB** (no cap hit) | **0.01 s — no permission prompt observed** | **no** (0 bytes in 2.5 s) | yes on the byte path (this is the emulator; the injection itself is measured through the pty, and the same reports are verified over SSH below) |
| **SSH session** (`sshd` in debian:bookworm-slim, key auth, `-tt`) | n/a (not the emulator) | n/a | n/a | **yes** — a copy issued on the remote host landed in the *local* WezTerm clipboard, whole, in **0.13 s** | 4,117 B tested, whole | 0.13 s | n/a in the bare-pty leg | **yes**: 13/13 reports injected here decoded remotely with the same kind mapping and the same **(1, 1)** coordinate offset; 1.20 s for the burst of 13 |
| **SSH without a remote pty** (`ssh host cmd`, `-T`) | — | — | — | bytes still arrive | — | — | — | the transport is not the problem: **21/21 bytes arrived and 0 events were decoded**, because stdin on the far side is a pipe, not a tty |
| **Zed** (this agent's own terminal, `TERM_PROGRAM=zed`) | **not measured** — the agent's tty is its own UI; writing the leg's escape sequences into it would put garbage in the running session | — | — | — | — | — | — | run `python3 spikes/mouse_clipboard_e2e.py --in-terminal` inside Zed's terminal panel to fill this row |
| **iTerm2 / Ghostty / kitty** | **not installed on this machine**; nothing is claimed for them | — | — | — | — | — | — | `--launch` is a template (`PDL2_LAUNCH='open -a iTerm {file}'`), so the leg runs in any of them without touching the spike |

## What this says that the ADR has to price (measurements only, no decisions)

1. **OSC 52 is not one thing.** In WezTerm it is a 1 MiB, prompt-free, sub-10 ms
   copy. In Apple Terminal it does not exist. Any claim of the form "the clipboard
   works over OSC 52" has to be per-terminal, and the fallback path (a native
   helper) is not optional.
2. **Apple Terminal cannot be driven by the mouse at all** — measured as: the
   emulator answers nothing about `?1000/?1002/?1003/?1006`, so nothing that
   depends on receiving reports has a chance there. In that terminal the user's
   selection is the terminal's own and copy is the terminal's own.
3. **A short OSC 52 copy can lose its tail silently in principle; here it did not.**
   Every passing copy carried its own `<<END-PAYLOAD-n>>` marker through, which is
   what makes "Copied 48,120 characters" checkable rather than hopeful. The
   `Copied N` message should be printed from the bytes the app put on the wire
   **and** verified against the marker, or not printed at all.
4. **The write path needs no permission in the one modern terminal tested**, so a
   permission-prompt-shaped design (asking once, remembering) is priced at ~0 for
   WezTerm and at "unsupported" for Apple Terminal. Measured latency 0.01 s; the
   proxy is coarse and a human-answered prompt would show as >1.5 s.
5. **Read-back (`ESC]52;c?`) is not something to build on.** Answered by nobody in
   this environment, in either direction.
6. **A mouse-tracking vim turns our mouse off on the way out.** After any child
   with `mouse=a`, the app must re-assert `?1000/?1002/?1006` itself; a killed
   child leaves the alternate screen and the mouse modes on too, so the re-assert
   has to happen on the *return from any child*, not on a happy path.
7. **Injection is cheap and lossless**: 128 reports, one write, 0.3 ms, nothing
   merged. If a flick ever exceeds that, it will not be the wire that stops it.
   The unknown that `looprs-pdl.8` needs is still the flick's own burst shape.

## Filling the gaps, when someone with hands and other terminals is here

```sh
# the bare-pty groups, every time, no terminal needed
cargo build --examples
python3 spikes/mouse_clipboard_e2e.py              | tee spikes/results/mouse-clipboard-e2e.log
python3 spikes/mouse_clipboard_e2e.py --control    | tee spikes/results/mouse-clipboard-e2e-control.log
python3 spikes/mouse_clipboard_e2e.py ssh          | tee spikes/results/mouse-clipboard-ssh.log

# the emulator leg, inside whatever terminal you are actually sitting in
python3 spikes/mouse_clipboard_e2e.py --in-terminal \
    | tee spikes/results/emulator-leg-$(echo "${TERM_PROGRAM:-unknown}").log

# the two claims that need a hand: three trackpad flicks, then shift-drag, then y/n
python3 spikes/mouse_clipboard_e2e.py --in-terminal --record-flick \
    | tee spikes/results/flick-$(echo "${TERM_PROGRAM:-unknown}").log

# the leg in another emulator without editing anything
PDL2_LAUNCH='open -a iTerm {file}' PDL2_LABEL=iterm \
    python3 spikes/mouse_clipboard_e2e.py --launch \
    | tee spikes/results/emulator-leg-iterm.log

# and the same with the remote-emitted OSC 52, which is the SSH-plus-clipboard row
PDL2_LAUNCH='/Applications/WezTerm.app/Contents/MacOS/wezterm start --always-new-process -- {file}' \
    PDL2_LABEL=wezterm-ssh python3 spikes/mouse_clipboard_e2e.py --launch --ssh-emulator \
    | tee spikes/results/emulator-leg-wezterm-ssh.log
```
