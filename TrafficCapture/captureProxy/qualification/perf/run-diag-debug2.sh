#!/usr/bin/env bash
# DIAGNOSTIC ONLY: librdkafka produce pipelining under the 750 ms toxic, with the debug filter raised.
set -u
H=/tmp/rust-proxy-qual/harness; L=/tmp/rust-proxy-qual/logs
while pgrep -f 'run-paired.sh gprodrate' >/dev/null; do sleep 20; done
export RUST_PROXY_IMAGE=localhost:5001/migrations/capture_proxy:rust-03ddf2ab2
KAFKA_PARTITIONS=4 KAFKA_PROPS_FILE=/tmp/rust-proxy-qual/diag-kafka-debug2.properties \
  DIAG_COMPONENT_LOG_LEVELS=librdkafka=debug \
  "$H/run-profile-diag.sh" diag2-rtt-rust-p4-debug rust kafka-delayed 30 15 > $L/diag2-rtt-rust-p4-debug.log 2>&1
echo "DIAG2_DONE rc=$?"
