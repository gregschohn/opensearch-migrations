#!/bin/sh
# cgroup v2 sampler. Runs in a sidecar container with --cgroupns=host and
# /sys/fs/cgroup bind-mounted read-only, pinned to cores that the proxy does
# not use.
#
# It MUST NOT run via `docker exec` into the proxy container: an exec'd process
# joins the target container's cgroup, so its own CPU time would be added to
# the very cpu.stat counter being sampled. Reading from a separate cgroup in
# the host cgroup namespace keeps the sampler's cost out of the measurement.
#
# Usage: cgsample.sh <container-id> <interval-seconds> <out.csv>
set -eu
CID="$1"; INTERVAL="$2"; OUT="$3"

CG="/hostcg/docker/$CID"
if [ ! -d "$CG" ]; then
  CG=$(find /hostcg -maxdepth 5 -type d -name "*$CID*" 2>/dev/null | head -1 || true)
fi
if [ -z "$CG" ] || [ ! -r "$CG/cpu.stat" ]; then
  echo "cgsample: cannot find readable cgroup for $CID" >&2
  exit 1
fi
echo "cgsample: using $CG" >&2

fld() { awk -v k="$2" '$1==k{print $2; found=1} END{if(!found) print "NA"}' "$1"; }

# VM-wide CPU is sampled alongside the cgroup counters because the Linux
# network stack's softirq work for a container's packets is NOT charged to that
# container's cgroup. Without this column, "cores used" would silently exclude
# the per-packet cost of the veth/bridge path, which for a proxy is a large
# share of the real system cost. It is recorded for both implementations so the
# comparison stays fair, and reported separately so the omission is visible.
printf 'epoch_ms,usage_usec,user_usec,system_usec,nr_periods,nr_throttled,throttled_usec,memory_current,memory_peak,anon,file,inactive_anon,active_anon,slab,pgmajfault,vm_user,vm_nice,vm_system,vm_idle,vm_iowait,vm_irq,vm_softirq,vm_steal\n' > "$OUT"

# busybox `date` ignores %3N and silently emits seconds only, which would make
# every timestamp 1000x too small and silently empty the measured window. Probe
# once and pick a formatter that is known to produce milliseconds.
if [ "$(date +%s%3N 2>/dev/null | wc -c)" -ge 14 ]; then
  now_ms() { date +%s%3N; }
else
  now_ms() { echo "$(date +%s)000"; }
  echo "cgsample: date has no %3N; using whole-second timestamps" >&2
fi

while :; do
  [ -r "$CG/cpu.stat" ] || break
  NOW=$(now_ms)
  CS="$CG/cpu.stat"; MS="$CG/memory.stat"
  U=$(fld "$CS" usage_usec);   UU=$(fld "$CS" user_usec);  SU=$(fld "$CS" system_usec)
  NP=$(fld "$CS" nr_periods);  NT=$(fld "$CS" nr_throttled); TU=$(fld "$CS" throttled_usec)
  MC=$(cat "$CG/memory.current" 2>/dev/null || echo NA)
  MP=$(cat "$CG/memory.peak"    2>/dev/null || echo NA)
  AN=$(fld "$MS" anon);         FI=$(fld "$MS" file)
  IA=$(fld "$MS" inactive_anon); AA=$(fld "$MS" active_anon)
  SL=$(fld "$MS" slab);         PF=$(fld "$MS" pgmajfault)
  VM=$(awk '/^cpu /{print $2","$3","$4","$5","$6","$7","$8","$9; exit}' /proc/stat)
  printf '%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
    "$NOW" "$U" "$UU" "$SU" "$NP" "$NT" "$TU" "$MC" "$MP" "$AN" "$FI" "$IA" "$AA" "$SL" "$PF" "$VM" >> "$OUT"
  sleep "$INTERVAL"
done
