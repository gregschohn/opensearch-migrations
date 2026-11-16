#!/usr/bin/env bash
# Long p99 batch: 18 more pairs each on the gate-3 profiles (-> 28 total).
set -u
H=/tmp/rust-proxy-qual/harness
export ARM_A_LABEL=java ARM_B_LABEL=rust
for prof in double-tls churn23-tls; do
  ARM_A_ENV=$'MEASUREMENT_PATCH=1' ARM_B_ENV=$'' "$H/run-paired.sh" g3l "$prof" 18 java rust 30 15
done
echo GATES_LONG_DONE
