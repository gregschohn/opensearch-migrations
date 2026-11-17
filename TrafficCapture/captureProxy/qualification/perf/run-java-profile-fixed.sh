#!/usr/bin/env bash
# R2: Java proxy perf profile at the same fixed rate as run-rust-profile-fixed.sh (kernel vs user
# split by thread; JIT frames stay unresolved). Attribution only.
set -u
H=/tmp/rust-proxy-qual/harness; PF=/tmp/rust-proxy-qual/profiles/java-fixed
mkdir -p $PF
RID=jprof-fixed-$(date +%s)
export KAFKA_PARTITIONS=4 RATE_OVERRIDE=11371
"$H/run-profile.sh" $RID java plaintext-capture 60 20 > /tmp/rust-proxy-qual/logs/$RID.log 2>&1 &
RP=$!
until docker inspect gen-$RID >/dev/null 2>&1; do sleep 1; done
sleep 25
PID=$(docker exec px-$RID sh -c 'for p in /proc/[0-9]*; do grep -q java $p/comm 2>/dev/null && basename $p && break; done')
docker run --rm --privileged --pid=container:px-$RID --cpuset-cpus=16-17 \
  -v $PF:/out qualperf:1 perf record -F 999 -p ${PID:-1} -o /out/flat.data -- sleep 40 > $PF/record.log 2>&1
wait $RP; echo "profile run exit=$?"
docker run --rm --privileged -v $PF:/out qualperf:1 perf report -i /out/flat.data --kallsyms /proc/kallsyms \
  --stdio --no-children --sort comm,dso --percent-limit 0.5 > $PF/by-thread-dso.txt 2>/dev/null
docker run --rm --privileged -v $PF:/out qualperf:1 perf report -i /out/flat.data --kallsyms /proc/kallsyms \
  --stdio --no-children --sort sym --percent-limit 0.6 > $PF/kernel-syms.txt 2>/dev/null
cp /tmp/rust-proxy-qual/results/$RID/summary.json $PF/summary.json
echo JPROF_FIXED_DONE
