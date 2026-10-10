# Shared probe script for ADR-0001 (Bash terminal state: pipes vs PTY).
#
# The SAME script is fed line-by-line to a live bash by BOTH options in
# examples/spike_bash.rs, so the only difference between a `pipes` run and a
# `pty` run is how that bash was created. Output lines starting with `PROBE ` are
# extracted by the harness; every line is stamped with its arrival time, so
# timing-sensitive probes (Ctrl-C) are read off the trace, not off a report.
#
# Harness control lines (NOT sent to the shell):
#   #wait <ms>        pause the harness for <ms>
#   #raw <bytes>      send raw bytes; \xNN, \r, \n, \t are decoded
#   #resize <r> <c>   resize the child's pty (Option B only; Option A reports it)
#
# Blank lines and any other comment line are skipped.

# --- 1. Ctrl-C: does 0x03 reach the running command? --------------------------
# FIRST section on purpose: the harness races ahead of the shell, so keeping this at
# t~0 means `sleep 6` really is the foreground command when the 0x03 lands.
#   ~1.5s between the two PROBE lines => the tty line discipline sent SIGINT.
#   ~6.0s                                    => the byte sat in a buffer unread.
echo "PROBE pre_interrupt"
sleep 6
#wait 1500
#raw \x03
#wait 300
echo "PROBE post_interrupt"

# --- 2. Is the child attached to a terminal at all? ---------------------------
[ -t 0 ] && echo "PROBE stdin=tty" || echo "PROBE stdin=piped"
[ -t 1 ] && echo "PROBE stdout=tty" || echo "PROBE stdout=piped"
[ -t 2 ] && echo "PROBE stderr=tty" || echo "PROBE stderr=piped"

# --- 3. The /dev/tty escape hatch ---------------------------------------------
# If the child's controlling terminal is the SAME device as the harness's, then
# anything the child writes to /dev/tty -- sudo's password prompt, ssh's host-key
# confirmation, a pager's "press any key" -- lands directly on the real screen and
# bypasses looprs' renderer AND its input pipeline. We cannot render it, we cannot
# feed it, and it scribbles over our inline viewport.
child_tty=$(ps -o tty= -p $$ 2>/dev/null | tr -d ' ')
parent_tty=$(ps -o tty= -p $PPID 2>/dev/null | tr -d ' ')
echo "PROBE child_tty=${child_tty:-none} harness_tty=${parent_tty:-none} same=$([ "$child_tty" = "$parent_tty" ] && echo YES || echo no)"
# `(: <>/dev/tty)` actually opens the device; a stat/`-e` test always says the
# special file exists, so it proves nothing.
if ( : <>/dev/tty ) 2>/dev/null; then
  echo "PROBE dev_tty=openable=yes"
else
  echo "PROBE dev_tty=openable=no"
fi
# We deliberately do NOT run a live sudo/getpass password prompt here: it reads
# /dev/tty, which in the pipes case means it would eat the rest of this script.
# `dev_tty=openable` above is the proxy for "where would that prompt go".

# --- 4. Persistence across separate writes (the whole point of a shell mode) ---
echo "PROBE cwd_before=$(pwd)"
cd /tmp
echo "PROBE cwd_after_cd=$(pwd)"
export LOOPRS_SPIKE=42
echo "PROBE env_across_writes=$LOOPRS_SPIKE"
alias ll='echo alias-works'
ll

# --- 5. Job control -----------------------------------------------------------
sleep 3 &
sleep 0.5
echo "PROBE jobs_visible=$(jobs -l 2>/dev/null | wc -l | tr -d ' ')"

# --- 6. stdout / stderr interleaving ------------------------------------------
# Option A merges two pipes with two reader threads: whoever wakes first writes
# first, so the merged order is a scheduling race, not the program's order.
for i in 1 2 3 4 5 6; do echo "PROBE o$i-stdout"; echo "PROBE e$i-stderr" >&2; done

# --- 7. Terminal size, and does a resize reach the child? ----------------------
echo "PROBE stty_before=$(stty size 2>&1 | tr -d '\n')"
echo "PROBE env_size=COLUMNS=${COLUMNS:-unset} LINES=${LINES:-unset}"
# wait for the shell to catch up with the harness before resizing, so
# `stty_before` really is measured before the resize.
#wait 1500
#resize 40 120
#wait 500
echo "PROBE stty_after_resize=$(stty size 2>&1 | tr -d '\n')"

# --- 8. A real full-screen program: vim ---------------------------------------
rm -f /tmp/.looprs_spike_vim.txt.swp /tmp/looprs_spike_vim.txt
#wait 400
vim -u NONE -i NONE -n -X -c 'set noswapfile' /tmp/looprs_spike_vim.txt
#wait 2500
#raw iHELLO-FROM-VIM\r
#wait 800
#raw \x1b:wq!\r
#wait 1500
echo "PROBE vim_returned=$?"
echo "PROBE vim_wrote_file=[$(cat /tmp/looprs_spike_vim.txt 2>/dev/null)]"

# --- 9. sudo (last: a password-reading sudo slurps queued tty input) ---------
sudo -k 2>/dev/null
sudo -n true >/tmp/looprs_spike_sudo.out 2>&1
echo "PROBE sudo_n exit=$? msg=$(head -1 /tmp/looprs_spike_sudo.out)"
# -S is the ONLY way a shell without a tty could ever be handed a password: the
# password has to come down a pipe, typed into OUR input box. Kept last because a
# password-reading sudo also slurps queued tty input.
printf 'not-a-real-password\n' | sudo -S true >/tmp/looprs_spike_sudo_s.out 2>&1
echo "PROBE sudo_S exit=$? msg=$(head -1 /tmp/looprs_spike_sudo_s.out)"
echo "PROBE done"
exit
