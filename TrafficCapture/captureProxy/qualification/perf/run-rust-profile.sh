#!/usr/bin/env bash
# Rust CPU profile on the PROFILING image (symbols kept). Never used for gate numbers.
set -u
H=/tmp/rust-proxy-qual/harness; R=/tmp/rust-proxy-qual/results; PF=/tmp/rust-proxy-qual/profiles/rust
mkdir -p $PF
RID=rprof-plaintext-capture
export RUST_PROXY_IMAGE=localhost:5001/migrations/capture_proxy:rust-b60dc220e-profiling
"$H/run-profile.sh" $RID rust plaintext-capture 60 20 > /tmp/rust-proxy-qual/logs/$RID.log 2>&1 &
RP=$!
until docker inspect gen-$RID >/dev/null 2>&1; do sleep 1; done   # generator started
sleep 25                                                          # warm-up + margin
# Flat self-time profile of every proxy thread, user + kernel, 40 s at 999 Hz,
# from a privileged sidecar on the infra cores sharing the proxy PID namespace.
docker run --rm --privileged --pid=container:px-$RID --cpuset-cpus=16-17 \
  -v $PF:/out qualperf:1 perf record -F 999 -p 1 -o /out/flat.data -- sleep 40 > $PF/record-flat.log 2>&1
wait $RP; echo "profile run exit=$?"
docker run --rm --privileged -v $PF:/out -v /tmp/rust-proxy-qual/rustbin-prof:/symfs:ro qualperf:1 \
  perf report -i /out/flat.data --symfs /symfs --stdio --no-children --sort dso,symbol --percent-limit 0.3 \
  > $PF/flat-report.txt 2> $PF/flat-report.err
docker run --rm --privileged -v $PF:/out -v /tmp/rust-proxy-qual/rustbin-prof:/symfs:ro qualperf:1 \
  perf report -i /out/flat.data --symfs /symfs --stdio --no-children --sort comm --percent-limit 0.5 \
  > $PF/by-thread.txt 2>/dev/null
echo RPROF_DONE
