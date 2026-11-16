#!/usr/bin/env bash
# DIAGNOSTIC (single Rust runs, not gate numbers): effect of --kafka-producer-count on the
# producer window, slow broker (kafka-delayed) and healthy broker at Gate 5 load (kafka-healthy).
set -u
H=/tmp/rust-proxy-qual/harness; L=/tmp/rust-proxy-qual/logs
export RUST_PROXY_IMAGE=localhost:5001/migrations/capture_proxy:rust-${MP_SHA:?}
for n in 1 4 8; do
  PROXY_EXTRA_ARGS="--kafka-producer-count $n" \
    "$H/run-profile.sh" mpd-delayed-n$n rust kafka-delayed 30 15 > $L/mpd-delayed-n$n.log 2>&1
  echo "delayed n=$n rc=$?"
done
for n in 1 4 8; do
  PROXY_EXTRA_ARGS="--kafka-producer-count $n" KAFKA_PROPS_FILE=/tmp/rust-proxy-qual/diag-kafka-stats.properties \
    "$H/run-profile-diag.sh" mpd-healthy-n$n rust kafka-healthy 30 15 > $L/mpd-healthy-n$n.log 2>&1
  echo "healthy n=$n rc=$?"
done
echo MPD_DONE
