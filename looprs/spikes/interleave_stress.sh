# Stress version of the interleaving probe: same alternating stdout/stderr markers,
# but each marker is followed by 4 KiB of filler on the SAME fd, so both pipes are
# busy and the two reader threads in Option A actually contend.
#
#   for i in $(seq 1 4); do
#     LOOPRS_SPIKE_PROBES=spikes/interleave_stress.sh \
#       script -q /dev/null cargo run -q --example spike_bash -- pipes \
#       | tr -d '\r' | grep 'merged in program order'
#   done
N=25
FILLER=$(head -c 4096 /dev/zero | tr '\0' 'x')
for i in $(seq 1 $N); do
  echo "PROBE o$i-stdout"
  echo "$FILLER"
  echo "PROBE e$i-stderr" >&2
  echo "$FILLER" >&2
done
echo "PROBE done"
