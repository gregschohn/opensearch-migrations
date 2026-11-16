#!/usr/bin/env bash
# DIAGNOSTIC ONLY (not gate numbers): two-round-trip ack under a 750 ms toxic.
set -u
H=/tmp/rust-proxy-qual/harness; L=/tmp/rust-proxy-qual/logs
export RUST_PROXY_IMAGE=localhost:5001/migrations/capture_proxy:rust-03ddf2ab2
for p in 8 4; do for i in 1 2; do
  KAFKA_PARTITIONS=$p "$H/run-profile.sh" diag-rtt-rust-p$p-r$i rust kafka-delayed 30 15 > $L/diag-rtt-rust-p$p-r$i.log 2>&1
done; done
KAFKA_PARTITIONS=4 MEASUREMENT_PATCH=1 "$H/run-profile.sh" diag-rtt-java-p4-r1 java kafka-delayed 30 15 > $L/diag-rtt-java-p4-r1.log 2>&1
KAFKA_PARTITIONS=8 KAFKA_PROPS_FILE=/tmp/rust-proxy-qual/diag-kafka-debug.properties \
  "$H/run-profile.sh" diag-rtt-rust-p8-debug rust kafka-delayed 30 15 > $L/diag-rtt-rust-p8-debug.log 2>&1
echo DIAG_DONE
