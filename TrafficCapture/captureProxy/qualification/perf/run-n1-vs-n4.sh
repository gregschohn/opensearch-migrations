#!/usr/bin/env bash
# Direct Rust 1-producer vs Rust 4-producer pairs, Gate 5 saturating profile, 4 partitions.
set -u
H=/tmp/rust-proxy-qual/harness; L=/tmp/rust-proxy-qual/logs
until grep -q MATRIX_DONE $L/patched-chain.log 2>/dev/null; do sleep 30; done
export RUST_PROXY_IMAGE=localhost:5001/migrations/capture_proxy:rust-428ef064c KAFKA_PARTITIONS=4
ARM_A_LABEL=rust1 ARM_B_LABEL=rust4 \
ARM_A_ENV='PROXY_EXTRA_ARGS=--kafka-producer-count 1' ARM_B_ENV='PROXY_EXTRA_ARGS=--kafka-producer-count 4' \
  "$H/run-paired.sh" pm-n1vn4 kafka-healthy 10 rust rust 30 15
echo N1VN4_DONE
