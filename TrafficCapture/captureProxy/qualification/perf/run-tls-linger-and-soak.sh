#!/usr/bin/env bash
# (a) production shape (TLS both sides, 23% churn, capture) at 2,845 req/s: Rust linger 0 vs 0.5
# (b) the same, Java vs Rust linger 0.5
# (c) heartbeat soak: 24 partitions, plaintext capture flat out, 10 minutes, default H/E
set -u
H=/tmp/rust-proxy-qual/harness
IMG=localhost:5001/migrations/capture_proxy:rust-c4d825c05
export KAFKA_PARTITIONS=4 RUST_PROXY_IMAGE=$IMG
ARM_A_LABEL=l0 ARM_B_LABEL=l05 ARM_A_ENV='RATE_OVERRIDE=2845' \
ARM_B_ENV=$'RATE_OVERRIDE=2845\nKAFKA_PROPS_FILE=/tmp/rust-proxy-qual/linger-0.5.properties' \
  "$H/run-paired.sh" tl-rust prodshape-tls 6 rust rust 30 15; echo "BATCH_DONE tl-rust"
ARM_A_LABEL=java ARM_B_LABEL=rustl05 ARM_A_ENV=$'MEASUREMENT_PATCH=1\nRATE_OVERRIDE=2845' \
ARM_B_ENV=$'RATE_OVERRIDE=2845\nKAFKA_PROPS_FILE=/tmp/rust-proxy-qual/linger-0.5.properties' \
  "$H/run-paired.sh" tl-java prodshape-tls 5 java rust 30 15; echo "BATCH_DONE tl-java"
KAFKA_PARTITIONS=24 "$H/run-profile.sh" soak-hb24-$(date +%s) rust plaintext-capture 600 15 > /tmp/rust-proxy-qual/logs/soak-hb24.log 2>&1; echo "SOAK rc=$?"
echo TLSLS_DONE
