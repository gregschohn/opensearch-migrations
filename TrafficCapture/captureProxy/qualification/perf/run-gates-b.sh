#!/usr/bin/env bash
# Gates 3, 5, 6 throughput/memory axes (10 pairs each; gate 6 5 pairs).
set -u
H=/tmp/rust-proxy-qual/harness
export ARM_A_LABEL=java ARM_B_LABEL=rust
for spec in "g3 double-tls 10" "g3 churn23-tls 10" "g5 kafka-healthy 10" "g6 kafka-delayed 5"; do
  set -- $spec
  ARM_A_ENV=$'MEASUREMENT_PATCH=1' ARM_B_ENV=$'' "$H/run-paired.sh" "$1" "$2" "$3" java rust 30 15
done
echo GATES_B_DONE
