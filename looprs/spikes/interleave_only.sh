# Minimal repeatable probe for the stdout/stderr interleaving claim.
#
#   for i in $(seq 1 10); do
#     LOOPRS_SPIKE_PROBES=spikes/interleave_only.sh \
#       script -q /dev/null cargo run -q --example spike_bash -- pipes \
#       | tr -d '\r' | grep 'merged in program order'
#   done
#
# Each `echo` writes to a different fd. Option A merges those two fds with two
# reader threads, so the merged order is a scheduling race; Option B never has two
# streams to merge -- stdout and stderr are the same tty.
for i in 1 2 3 4 5 6 7 8 9 10 11 12; do
  echo "PROBE o$i-stdout"
  echo "PROBE e$i-stderr" >&2
done
echo "PROBE done"
