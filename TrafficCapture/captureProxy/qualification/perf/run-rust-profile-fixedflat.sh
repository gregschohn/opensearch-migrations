#!/usr/bin/env bash
# R2: Rust CPU profile at a FIXED rate (plaintext capture, 11,371 req/s, 4 partitions) on the
# profiling image. Attribution only; never used for gate numbers.
set -u
H=/tmp/rust-proxy-qual/harness; SHA=${PROF_SHA:?}; PF=/tmp/rust-proxy-qual/profiles/rust-fixedflat-$SHA
mkdir -p $PF
RID=rprof-fixedflat-$SHA
export RUST_PROXY_IMAGE=localhost:5001/migrations/capture_proxy:rust-$SHA-profiling KAFKA_PARTITIONS=4 RATE_OVERRIDE=11371
"$H/run-profile.sh" $RID rust plaintext-capture 60 20 > /tmp/rust-proxy-qual/logs/$RID.log 2>&1 &
RP=$!
until docker inspect gen-$RID >/dev/null 2>&1; do sleep 1; done
sleep 25
docker run --rm --privileged --pid=container:px-$RID --cpuset-cpus=16-17 \
  -v $PF:/out qualperf:1 perf record -F 999 -p 1 -o /out/flat.data -- sleep 40 > $PF/record.log 2>&1
wait $RP; echo "profile run exit=$?"
S=/tmp/rust-proxy-qual/rustbin-prof-$SHA
docker run --rm --privileged -v $PF:/out -v $S:/symfs:ro qualperf:1 \
  perf report -i /out/flat.data --symfs /symfs --kallsyms /proc/kallsyms --stdio --no-children --sort dso,symbol --percent-limit 0.3  \
  > $PF/flat-report.txt 2> $PF/flat-report.err
docker run --rm --privileged -v $PF:/out -v $S:/symfs:ro qualperf:1 \
  perf report -i /out/flat.data --symfs /symfs --kallsyms /proc/kallsyms --stdio --no-children --sort comm --percent-limit 0.5  \
  > $PF/by-thread.txt 2>/dev/null
docker run --rm --privileged -v $PF:/out -v $S:/symfs:ro qualperf:1 \
  perf report -i /out/flat.data --symfs /symfs --kallsyms /proc/kallsyms --stdio --children --sort symbol --percent-limit 2 \
  > $PF/children-report.txt 2>/dev/null
echo RPROF_FIXED_DONE
