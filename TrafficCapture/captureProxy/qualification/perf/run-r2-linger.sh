#!/usr/bin/env bash
# R2: does a small linger.ms cut Kafka-side CPU? Fixed 11,371 req/s plaintext capture, 4 partitions, auto producers.
set -u
H=/tmp/rust-proxy-qual/harness
export KAFKA_PARTITIONS=4 RUST_PROXY_IMAGE=localhost:5001/migrations/capture_proxy:rust-87aa0fc89
for L in 0.5 2; do
ARM_A_LABEL=l0 ARM_B_LABEL=l$L ARM_A_ENV='RATE_OVERRIDE=11371' \
ARM_B_ENV=$'RATE_OVERRIDE=11371\nKAFKA_PROPS_FILE=/tmp/rust-proxy-qual/linger-'$L'.properties' \
  "$H/run-paired.sh" r2-linger$L plaintext-capture 6 rust rust 30 15
echo "BATCH_DONE linger $L"
done
echo R2L_DONE
