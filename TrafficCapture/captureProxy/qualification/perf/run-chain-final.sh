#!/usr/bin/env bash
# Phase 1 measurement chain on the final Rust image. Waits for the in-flight
# gate-2 batch (rust-748379d05, no-capture, still valid) before starting.
set -u
H=/tmp/rust-proxy-qual/harness; L=/tmp/rust-proxy-qual/logs
export RUST_PROXY_IMAGE=localhost:5001/migrations/capture_proxy:rust-2ff6080df
export ARM_A_LABEL=java ARM_B_LABEL=rust
# probe (probe-2ff) and the 748379d05 gate-2 batch already complete.
ARM_A_ENV=$'MEASUREMENT_PATCH=1' ARM_B_ENV=$'' "$H/run-paired.sh" g2c plaintext-nocapture 3 java rust 30 15
ARM_A_ENV=$'MEASUREMENT_PATCH=1' ARM_B_ENV=$'' "$H/run-paired.sh" g4 plaintext-capture 10 java rust 30 15
ARM_A_LABEL=javapin ARM_A_ENV=$'MEASUREMENT_PATCH=1\nHEAP_OPTS=-Xms536m -Xmx536m' ARM_B_ENV=$'' \
  "$H/run-paired.sh" g4pin plaintext-capture 5 java rust 30 15
echo GATES_2_4_DONE
for spec in "g3 double-tls 10" "g3 churn23-tls 10" "g5 kafka-healthy 10" "g6 kafka-delayed 5"; do
  set -- $spec
  ARM_A_LABEL=java ARM_A_ENV=$'MEASUREMENT_PATCH=1' ARM_B_ENV=$'' "$H/run-paired.sh" "$1" "$2" "$3" java rust 30 15
done
echo GATES_3_5_6_DONE
for prof in double-tls churn23-tls; do
  ARM_A_LABEL=java ARM_A_ENV=$'MEASUREMENT_PATCH=1' ARM_B_ENV=$'' "$H/run-paired.sh" g3l "$prof" 18 java rust 30 15
done
echo GATES_LONG_DONE
