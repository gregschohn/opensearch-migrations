#!/usr/bin/env bash
# Phase 1 measurement chain on the instrument-parity Rust image.
#   run-chain-v3.sh <rust-image-tag>
set -u
H=/tmp/rust-proxy-qual/harness; L=/tmp/rust-proxy-qual/logs; R=/tmp/rust-proxy-qual/results
export RUST_PROXY_IMAGE="${1:?rust image required}"
export ARM_A_LABEL=java ARM_B_LABEL=rust
J=$'MEASUREMENT_PATCH=1'
docker image inspect "$RUST_PROXY_IMAGE" >/dev/null 2>&1 || { echo "NO_IMAGE"; exit 1; }

# Instrument-parity probe (not recorded): every instrument the Java arm exports
# for the gates must reach the collector from the Rust arm, and the four gate
# instruments must carry the 17 GATE_LATENCY_BUCKETS_MS bounds.
"$H/run-profile.sh" probe-v3 rust plaintext-capture 15 10 > $L/probe-v3.log 2>&1
P=$R/probe-v3/otel-metrics.prom
miss=""
for m in fullRequest_count_total blockingRequest_count_total bytesRead_bytes_total bytesWritten_bytes_total \
         kafkaCommitDuration_milliseconds_bucket blockedDuration_milliseconds_bucket \
         gatheringRequestDuration_milliseconds_bucket gatheringResponseDuration_milliseconds_bucket; do
  grep -q "^$m{" "$P" 2>/dev/null || miss="$miss $m"
done
want="0.1 0.25 0.5 1 2.5 5 10 25 50 100 250 500 1000 2500 5000 10000 30000 +Inf"
for m in kafkaCommitDuration blockedDuration gatheringRequestDuration gatheringResponseDuration; do
  got=$(grep "^${m}_milliseconds_bucket{" "$P" 2>/dev/null | sed 's/.*le="\([^"]*\)".*/\1/' | tr '\n' ' ' | sed 's/ $//')
  [[ "$got" == "$want" ]] || miss="$miss ${m}[buckets:$got]"
done
if [[ -n "$miss" ]]; then echo "PROBE_FAIL:$miss"; exit 2; fi
echo PROBE_OK

pr() { ARM_A_ENV="$J" ARM_B_ENV='' "$H/run-paired.sh" "$@"; }
if [[ "${TOPUPS_ONLY:-0}" != 1 ]]; then
# 1. Confirmation on the final image (primary evidence stays the 9/10-pair runs).
pr g2f plaintext-nocapture 3 java rust 30 15
pr g4f plaintext-capture 3 java rust 30 15
echo CONFIRM_DONE
# 2. Gates 3, 5, 6.
pr g3f double-tls 10 java rust 30 15
pr g3f churn23-tls 10 java rust 30 15
pr g5f kafka-healthy 10 java rust 30 15
pr g6f kafka-delayed 5 java rust 30 15
echo GATES_3_5_6_DONE
fi
[[ "${RUN_TOPUPS:-0}" == 1 ]] || exit 0
# 3. p99 top-ups: gate 3 to 28 pairs, gate 5 ack p99 to 20 pairs.
pr g3fl double-tls 18 java rust 30 15
pr g3fl churn23-tls 18 java rust 30 15
pr g5fl kafka-healthy 10 java rust 30 15
echo GATES_LONG_DONE
