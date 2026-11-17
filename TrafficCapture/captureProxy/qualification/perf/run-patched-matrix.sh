#!/usr/bin/env bash
# Paired Java/Rust matrix on the patched-librdkafka image: 1 producer vs 4 producers.
# 4 partitions everywhere (both arms), so 4 producers = exactly one partition per producer, and the
# per-proxy partition count matches production (512 partitions / 128 proxies).
# Rates are the same targets used before (derived earlier from Java saturating arms), so the
# fixed-rate results stay comparable with the Phase 1 report.
set -u
H=/tmp/rust-proxy-qual/harness; L=/tmp/rust-proxy-qual/logs
export RUST_PROXY_IMAGE=localhost:5001/migrations/capture_proxy:rust-428ef064c KAFKA_PARTITIONS=4
export ARM_A_LABEL=java
batch() {  # tag profile pairs rate(or empty) n
  local tag=$1 profile=$2 pairs=$3 rate=$4 n=$5
  local aenv=$'MEASUREMENT_PATCH=1' benv="PROXY_EXTRA_ARGS=--kafka-producer-count $n"
  if [[ -n "$rate" ]]; then aenv+=$'\n'"RATE_OVERRIDE=$rate"; benv+=$'\n'"RATE_OVERRIDE=$rate"; fi
  ARM_B_LABEL=rust$n ARM_A_ENV="$aenv" ARM_B_ENV="$benv" \
    "$H/run-paired.sh" "$tag" "$profile" "$pairs" java rust 30 15
  echo "BATCH_DONE $tag $profile n=$n"
}
for n in 1 4; do batch pm-g5sat-n$n  kafka-healthy     10 ""    $n; done
for n in 1 4; do batch pm-g6-n$n     kafka-delayed      5 ""    $n; done
for n in 1 4; do batch pm-g5rate-n$n kafka-healthy     10 15000 $n; done
for n in 1 4; do batch pm-g4rate-n$n plaintext-capture  5 11371 $n; done
for n in 1 4; do batch pm-gprod-n$n  prodshape-tls      5 2845  $n; done
for n in 1 4; do batch pm-g4sat-n$n  plaintext-capture  5 ""    $n; done
echo MATRIX_DONE
