#!/usr/bin/env bash
# R2 diagnostic: delivery relay thread (rust-e84c1750c) vs direct delivery route (rust-87aa0fc89),
# plaintext capture at a fixed 11,371 req/s, 4 partitions.
set -u
H=/tmp/rust-proxy-qual/harness
export KAFKA_PARTITIONS=4
ARM_A_LABEL=relay ARM_B_LABEL=direct \
ARM_A_ENV=$'RATE_OVERRIDE=11371\nRUST_PROXY_IMAGE=localhost:5001/migrations/capture_proxy:rust-e84c1750c' \
ARM_B_ENV=$'RATE_OVERRIDE=11371\nRUST_PROXY_IMAGE=localhost:5001/migrations/capture_proxy:rust-87aa0fc89' \
  "$H/run-paired.sh" r2-relay plaintext-capture 6 rust rust 30 15
echo R2R_DONE
