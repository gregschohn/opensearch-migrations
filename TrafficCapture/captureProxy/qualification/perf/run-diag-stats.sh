#!/usr/bin/env bash
# DIAGNOSTIC ONLY: librdkafka per-broker window statistics on a healthy broker at Gate 5 load.
set -u
H=/tmp/rust-proxy-qual/harness; L=/tmp/rust-proxy-qual/logs
until grep -q G4RATE_DONE $L/prod2-driver.log 2>/dev/null; do sleep 15; done
export RUST_PROXY_IMAGE=localhost:5001/migrations/capture_proxy:rust-03ddf2ab2
KAFKA_PROPS_FILE=/tmp/rust-proxy-qual/diag-kafka-stats.properties \
  "$H/run-profile-diag.sh" diag3-stats-rust-healthy rust kafka-healthy 30 15 > $L/diag3-stats-rust-healthy.log 2>&1
echo "DIAG3_DONE rc=$?"
