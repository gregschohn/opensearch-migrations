#!/usr/bin/env bash
# Confirmation pairs on the automatic-sizing image (default flags), Java arms on the measurement overlay.
set -u
H=/tmp/rust-proxy-qual/harness
export RUST_PROXY_IMAGE=localhost:5001/migrations/capture_proxy:rust-e84c1750c ARM_A_LABEL=java ARM_B_LABEL=rustauto
KAFKA_PARTITIONS=4 ARM_A_ENV=$'MEASUREMENT_PATCH=1' ARM_B_ENV='' \
  "$H/run-paired.sh" ac-g5sat-p4 kafka-healthy 5 java rust 30 15; echo "BATCH_DONE ac-g5sat-p4"
KAFKA_PARTITIONS=8 ARM_A_ENV=$'MEASUREMENT_PATCH=1\nRATE_OVERRIDE=2845' ARM_B_ENV='RATE_OVERRIDE=2845' \
  "$H/run-paired.sh" ac-gprod-p8 prodshape-tls 5 java rust 30 15; echo "BATCH_DONE ac-gprod-p8"
KAFKA_PARTITIONS=4 ARM_A_ENV=$'MEASUREMENT_PATCH=1\nRATE_OVERRIDE=2845' ARM_B_ENV='RATE_OVERRIDE=2845' \
  "$H/run-paired.sh" ac-gprod-p4 prodshape-tls 5 java rust 30 15; echo "BATCH_DONE ac-gprod-p4"
echo AUTO_CONFIRM_DONE
