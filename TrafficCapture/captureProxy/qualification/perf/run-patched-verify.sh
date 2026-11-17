#!/usr/bin/env bash
# DIAGNOSTIC (single Rust runs): patched librdkafka, 4 partitions, 1 vs 4 producers.
set -u
H=/tmp/rust-proxy-qual/harness; L=/tmp/rust-proxy-qual/logs
export RUST_PROXY_IMAGE=localhost:5001/migrations/capture_proxy:rust-428ef064c KAFKA_PARTITIONS=4
for n in 1 4; do
  PROXY_EXTRA_ARGS="--kafka-producer-count $n" "$H/run-profile.sh" pv-delayed-n$n rust kafka-delayed 30 15 > $L/pv-delayed-n$n.log 2>&1; echo "delayed n=$n rc=$?"
done
for n in 1 4; do
  PROXY_EXTRA_ARGS="--kafka-producer-count $n" KAFKA_PROPS_FILE=/tmp/rust-proxy-qual/diag-kafka-stats.properties \
    "$H/run-profile-diag.sh" pv-healthy-n$n rust kafka-healthy 30 15 > $L/pv-healthy-n$n.log 2>&1; echo "healthy n=$n rc=$?"
done
echo PV_DONE
