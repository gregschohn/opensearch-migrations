#!/usr/bin/env bash
# R2: Kafka-side CPU, 1 producer vs auto (4 on this single-broker, 4-partition bench), fixed 11,371 req/s.
set -u
H=/tmp/rust-proxy-qual/harness
export KAFKA_PARTITIONS=4 RUST_PROXY_IMAGE=localhost:5001/migrations/capture_proxy:rust-87aa0fc89
ARM_A_LABEL=n1 ARM_B_LABEL=auto \
ARM_A_ENV=$'RATE_OVERRIDE=11371\nPROXY_EXTRA_ARGS=--kafka-producer-count 1' ARM_B_ENV='RATE_OVERRIDE=11371' \
  "$H/run-paired.sh" r2-producers plaintext-capture 6 rust rust 30 15
echo R2P_DONE
