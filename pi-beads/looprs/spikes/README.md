# ADR-0001 spikes

Throwaway programs that prove or disprove the risky claims behind
[ADR-0001](../docs/adr/0001-bash-terminal-state-pty.md) (Bash terminal state: pipes vs a real
PTY). Nothing here is production code. Decisions live in the ADR; this directory is the
evidence, so a future reader can re-run it instead of re-arguing.

| File | What it is |
|---|---|
| `examples/spike_bash.rs` | The harness. `spawn_pipes()` (Option A) and `spawn_pty()` (Option B) are the only per-option code; everything after that is shared. |
| `spikes/probes.sh` | The probe script fed line-by-line to a live bash by **both** options. |
| `spikes/interleave_only.sh` | Small stdout/stderr-ordering probe, for repeating under contention. |
| `spikes/interleave_stress.sh` | Same, with 4 KiB filler per line so both pipes are busy. |
| `spikes/pi_bash_rpc.py` | Question 4: does pi's RPC `bash` command keep shell state? (It does not.) |
| `spikes/results/` | Committed raw output of the runs quoted in the ADR. |

## Running it

**Run under a real terminal.** The harness needs `/dev/tty` to exist and be openable, otherwise
the escape-hatch / vim / resize probes are meaningless. `script -q /dev/null <cmd>` allocates a
PTY for the harness on macOS/BSD (use `script -q -c "<cmd>" /dev/null` on Linux):

```sh
script -q /dev/null cargo run -q --example spike_bash -- pipes | tee spikes/results/pipes.log
script -q /dev/null cargo run -q --example spike_bash -- pty   | tee spikes/results/pty.log
python3 spikes/pi_bash_rpc.py
```

Useful knobs:

- `LOOPRS_SPIKE_BASH=/opt/homebrew/bin/bash` — try a bash other than the system 3.2.
- `LOOPRS_SPIKE_PROBES=spikes/interleave_only.sh` — run a different probe script.

A run prints a timestamped timeline of every line the child emitted (the timestamp is when the
*harness* saw the bytes, which is what makes the Ctrl-C timing probe readable), then the
extracted `PROBE` lines, then marker checks run against the raw byte stream.

Repeat a probe under contention, which is how the interleaving result was obtained:

```sh
for i in $(seq 1 10); do
  LOOPRS_SPIKE_PROBES=spikes/interleave_only.sh \
    script -q /dev/null cargo run -q --example spike_bash -- pipes \
    | tr -d '\r' | grep 'merged in program order'
done | sort | uniq -c
# 9 NO / 1 YES  ->  Option A's two-thread merge is a race
# (same loop with `-- pty`: 10 YES)
```

## Control lines understood in the probe scripts

```
#wait <ms>       pause the harness
#raw <bytes>     send raw bytes; \xNN, \r, \n, \t are decoded   (e.g. \x03 == Ctrl-C)
#resize <r> <c>  resize the child's pty (Option A reports "nothing to resize")
```

Other `#` lines and blank lines are skipped. Everything else is sent verbatim + `\n`.

## Gotchas worth knowing before editing the probes

- A pty's line discipline **echoes input when it is written**, so in the PTY run the input lines
  appear immediately, in a burst, without a shell prompt. In the pipes run the echo comes from
  bash itself as it reads. Don't read the echo as execution — read the `PROBE` output lines.
- The harness races ahead of the shell. Timing-sensitive probes therefore go **first** in
  `probes.sh`, so `sleep 6` really is the foreground command when the `0x03` lands.
- A live `sudo`/`getpass` password prompt reads `/dev/tty`. In the pipes option that means it
  eats the rest of the probe script, so the probes only ask about it (`dev_tty=openable`) and
  run the non-interactive `-n` / piped `-S` variants last.
- `stty size` before/after `#resize` needs a `#wait` in between: otherwise the shell has not
  caught up and "before" is measured after the resize.
