#!/usr/bin/env bash
# Compact per-gate report from one or more paired manifests (concatenated).
#   gate-report.sh <label> "<gate args>" manifest [manifest...]
set -u
H=/tmp/rust-proxy-qual/harness; R=/tmp/rust-proxy-qual/results
label="$1"; gates="$2"; shift 2
comb="$R/paired-combined-$label.manifest"; : > "$comb"
for m in "$@"; do cat "$m" >> "$comb"; done
# shellcheck disable=SC2086
python3 -I "$H/paired_ratio.py" "$comb" $gates > "$R/report-$label.txt" 2>&1
grep -E '^# paired|^# order|^(rps_per_core|cores_used|kafka_records_per_s_per_core|kafka_mib_per_s_per_core|requests_per_s|proxy_added_p50_ms|proxy_added_p99_ms|steady_ws_mib|anon_p50_mib|kafka_ack_p99_ms) |^  [a-z_]+ (ge|le) ' "$R/report-$label.txt"
