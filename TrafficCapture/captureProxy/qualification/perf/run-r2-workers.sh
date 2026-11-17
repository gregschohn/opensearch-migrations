#!/usr/bin/env bash
# R2 diagnostic: tokio workers 2x cores (default) vs 1x cores, plaintext capture at a fixed 11,371 req/s.
set -u
H=/tmp/rust-proxy-qual/harness
export RUST_PROXY_IMAGE=localhost:5001/migrations/capture_proxy:rust-e84c1750c KAFKA_PARTITIONS=4
ARM_A_LABEL=w10 ARM_B_LABEL=w5 ARM_A_ENV='RATE_OVERRIDE=11371' ARM_B_ENV=$'RATE_OVERRIDE=11371\nPROXY_EXTRA_ARGS=--num-threads 5' \
  "$H/run-paired.sh" r2-workers plaintext-capture 6 rust rust 30 15
echo R2W_DONE
