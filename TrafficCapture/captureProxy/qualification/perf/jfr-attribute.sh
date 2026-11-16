#!/usr/bin/env bash
# Flat CPU and allocation attribution from a JFR recording.
#
#   jfr-attribute.sh <recording.jfr> <outdir>
#
# The capture_proxy image has no `jfr` binary (headless Corretto ships only
# java/keytool/rmiregistry), so the recording is analysed on the host with the
# host's own Corretto 21 `jfr` tool.
set -euo pipefail
JFR_FILE="${1:?recording.jfr required}"
OUT="${2:?outdir required}"
mkdir -p "$OUT"
JAVA_HOME="${JAVA_HOME:-$(/usr/libexec/java_home -v 21)}"
JFR="$JAVA_HOME/bin/jfr"
[[ -x "$JFR" ]] || { echo "no jfr tool at $JFR" >&2; exit 1; }

base="$(basename "$JFR_FILE" .jfr)"
"$JFR" summary "$JFR_FILE" > "$OUT/$base.summary.txt"

# Top frame only -> which method the CPU was actually executing.
"$JFR" print --events ExecutionSample,NativeMethodSample --stack-depth 1 "$JFR_FILE" \
  > "$OUT/$base.exec-top1.txt"

# Deeper stacks -> bucket attribution by subsystem (a frame anywhere in the
# stack decides the bucket, with precedence, because e.g. protobuf encoding is
# always called from the capture path).
"$JFR" print --events ExecutionSample,NativeMethodSample --stack-depth 60 "$JFR_FILE" \
  > "$OUT/$base.exec-deep.txt"

"$JFR" print --events ObjectAllocationSample --stack-depth 40 "$JFR_FILE" \
  > "$OUT/$base.alloc.txt" 2>/dev/null || echo "(no ObjectAllocationSample events)" > "$OUT/$base.alloc.txt"

python3 -I "$(dirname "${BASH_SOURCE[0]}")/jfr_buckets.py" \
  "$OUT/$base.exec-top1.txt" "$OUT/$base.exec-deep.txt" "$OUT/$base.alloc.txt" \
  > "$OUT/$base.attribution.json"
echo "wrote $OUT/$base.attribution.json"
