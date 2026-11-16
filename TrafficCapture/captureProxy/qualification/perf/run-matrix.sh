#!/usr/bin/env bash
# Run the full profile matrix for one proxy implementation, REPS valid reps each.
#
#   run-matrix.sh <TAG> <java|rust|none-direct> <reps> [duration_s] [warmup_s] [profiles...]
#
# RUN_ID is <TAG>-<proxy>-<profile>-r<N>, so every run's containers, topic,
# consumer group and output directory are unique and reps can be re-run
# individually without disturbing anything already measured.
#
# A rep that validate_run.py rejects is RETRIED (up to MAX_ATTEMPTS) under a
# fresh RUN_ID suffix rather than being averaged in. The brief's rule is
# "discard and rerun anything measured under unexpected load"; on this host the
# two things that actually force a discard are Docker Desktop container-create
# stalls, which corrupt the time base, and the proxy's own fail-closed
# termination when the starved Kafka broker drops the producer connection.
# Every rejection is logged with its reason and counted, so the discard rate is
# part of the result rather than hidden.
set -uo pipefail
HARNESS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HARNESS_DIR/profiles.sh"

TAG="${1:?tag required}"
KIND="${2:?java|rust|none-direct}"
REPS="${3:-5}"
DUR="${4:-45}"
WARM="${5:-20}"
if [[ $# -ge 5 ]]; then shift 5; else shift $#; fi
PROFILES=("$@")
[[ ${#PROFILES[@]} -eq 0 ]] && PROFILES=("${ALL_PROFILES[@]}")

MAX_ATTEMPTS="${MAX_ATTEMPTS:-3}"
SETTLE="${SETTLE:-8}"
MATRIX_LOG="/tmp/rust-proxy-qual/logs/matrix-$TAG-$KIND.log"
: > "$MATRIX_LOG"
TOTAL_OK=0; TOTAL_REJECT=0
declare -a REJECTS=()

for prof in "${PROFILES[@]}"; do
  got=0
  attempt=0
  # Continue attempt numbering past any run directory that already exists for
  # this tag/kind/profile. A previous version restarted at a1 on every
  # invocation, so a top-up run silently OVERWROTE the first attempt's results
  # and inherited its stale REJECTED marker. Results are the one thing this
  # harness must never destroy.
  existing=0
  for d in "/tmp/rust-proxy-qual/results/$TAG-$KIND-$prof-a"*; do
    [[ -d "$d" ]] || continue
    n="${d##*-a}"
    [[ "$n" =~ ^[0-9]+$ ]] && (( n > existing )) && existing="$n"
  done
  attempt="$existing"
  maxattempt=$(( existing + REPS * MAX_ATTEMPTS ))
  while (( got < REPS && attempt < maxattempt )); do
    attempt=$((attempt+1))
    RID="$TAG-$KIND-$prof-a$attempt"
    echo "=== $RID (valid so far: $got/$REPS) ===" | tee -a "$MATRIX_LOG"
    if "$HARNESS_DIR/run-profile.sh" "$RID" "$KIND" "$prof" "$DUR" "$WARM" >>"$MATRIX_LOG" 2>&1; then
      got=$((got+1)); TOTAL_OK=$((TOTAL_OK+1))
      echo "    OK ($got/$REPS)" | tee -a "$MATRIX_LOG"
    else
      TOTAL_REJECT=$((TOTAL_REJECT+1))
      reason="$(grep -m1 -A4 '^REJECT ' "/tmp/rust-proxy-qual/results/$RID/run.log" 2>/dev/null | tr '\n' ' ' | tr -s ' ')"
      [[ -z "$reason" ]] && reason="$(grep -m1 'FATAL' "/tmp/rust-proxy-qual/results/$RID/run.log" 2>/dev/null | tr -s ' ')"
      REJECTS+=("$RID: ${reason:-unknown}")
      echo "    REJECTED: ${reason:-unknown}" | tee -a "$MATRIX_LOG"
    fi
    sleep "$SETTLE"
  done
  if (( got < REPS )); then
    echo "!!! $prof only produced $got/$REPS valid reps after $attempt attempts" | tee -a "$MATRIX_LOG"
  fi
done

echo "=== matrix done: $TOTAL_OK valid, $TOTAL_REJECT rejected ===" | tee -a "$MATRIX_LOG"
for r in "${REJECTS[@]:-}"; do [[ -n "$r" ]] && echo "  REJECT $r" | tee -a "$MATRIX_LOG"; done
