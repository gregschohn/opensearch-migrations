#!/usr/bin/env bash
# Interleaved paired A/B measurement.
#
#   run-paired.sh <TAG> <profile> <pairs> <armA_kind> <armB_kind> [duration_s] [warmup_s]
#
# Per-arm environment is supplied as a single string, e.g.
#   ARM_A_ENV=$'MEASUREMENT_PATCH=1' \
#   ARM_B_ENV=$'MEASUREMENT_PATCH=1\nHEAP_OPTS=-Xms536m -Xmx536m' \
#     ./run-paired.sh heapcmp plaintext-capture 10 java java
#
# Entries are NEWLINE-separated, not space-separated, so a value may itself
# contain spaces (JVM flag lists always do).
#
# WHY PAIRED, AND WHY INTERLEAVED
#
# Host background load on this machine drifts by a factor of five over tens of
# minutes (see the spec's variance section), which gives single-run
# requests/s a 12-26% spread per profile. That drift is COMMON MODE: it moves
# both implementations in the same direction at the same time. Comparing a
# Java median taken now against a Rust median taken an hour later exposes the
# full drift; comparing arm A against arm B *within the same few minutes* and
# then taking the distribution of those per-pair ratios cancels the shared
# component.
#
# So this script never reports a median-of-A against a median-of-B. It reports
# the DISTRIBUTION of per-pair ratios: median, min, max, and the fraction of
# pairs clearing a gate threshold. A gate passes only if the paired-ratio
# distribution clears it.
#
# ORDER ALTERNATION
#
# Within each pair the order is alternated (A,B then B,A then A,B ...), so that
# any systematic first-position effect -- page cache warmth, Docker image layer
# caching, the VM settling after the previous teardown -- averages out instead
# of landing consistently on one arm. The order actually used is recorded for
# every pair.
set -uo pipefail
HARNESS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RESULTS_DIR=/tmp/rust-proxy-qual/results

TAG="${1:?tag required}"
PROFILE="${2:?profile required}"
PAIRS="${3:?number of pairs required}"
ARM_A_KIND="${4:?arm A kind required}"
ARM_B_KIND="${5:?arm B kind required}"
DUR="${6:-45}"
WARM="${7:-20}"

ARM_A_ENV="${ARM_A_ENV:-}"
ARM_B_ENV="${ARM_B_ENV:-}"
ARM_A_LABEL="${ARM_A_LABEL:-A}"
ARM_B_LABEL="${ARM_B_LABEL:-B}"
MAX_RETRY_PER_ARM="${MAX_RETRY_PER_ARM:-2}"

LOG="/tmp/rust-proxy-qual/logs/paired-$TAG-$PROFILE.log"
: > "$LOG"
MANIFEST="/tmp/rust-proxy-qual/results/paired-$TAG-$PROFILE.manifest"
: > "$MANIFEST"

say() { echo "$*" | tee -a "$LOG"; }

# Run one arm until it yields a VALID run, up to MAX_RETRY_PER_ARM extra tries.
# Returns the RUN_ID of the valid run on stdout, or empty on failure.
run_arm() {
  local pair="$1" armlabel="$2" kind="$3" envstr="$4" slot="$5"
  local try rid
  local -a kv=()
  while IFS= read -r line; do
    [[ -n "$line" ]] && kv+=("$line")
  done <<< "$envstr"
  for ((try=1; try<=MAX_RETRY_PER_ARM+1; try++)); do
    rid="$TAG-$PROFILE-p${pair}-${armlabel}-t${try}"
    if env ${kv[@]+"${kv[@]}"} "$HARNESS_DIR/run-profile.sh" "$rid" "$kind" "$PROFILE" "$DUR" "$WARM" \
         >>"$LOG" 2>&1 \
       && [[ -s "$RESULTS_DIR/$rid/summary.json" && ! -e "$RESULTS_DIR/$rid/REJECTED" ]] \
       && python3 -I "$HARNESS_DIR/validate_run.py" "$RESULTS_DIR/$rid" >>"$LOG" 2>&1; then
      # Exit status alone is not trusted: a run interrupted by a signal once
      # exited 0 with no summary.json and was recorded as a valid arm. The
      # arm counts only if its summary exists and re-validates here.
      echo "$rid"
      return 0
    fi
    echo "      pair $pair arm $armlabel try $try REJECTED" >> "$LOG"
    sleep 5
  done
  return 1
}

say "=== paired run: tag=$TAG profile=$PROFILE pairs=$PAIRS"
say "    arm $ARM_A_LABEL: kind=$ARM_A_KIND env='$ARM_A_ENV'"
say "    arm $ARM_B_LABEL: kind=$ARM_B_KIND env='$ARM_B_ENV'"
say "    duration=${DUR}s warmup=${WARM}s"

ok_pairs=0
for ((i=1; i<=PAIRS; i++)); do
  # Alternate which arm goes first.
  if (( i % 2 == 1 )); then
    first_label="$ARM_A_LABEL"; first_kind="$ARM_A_KIND"; first_env="$ARM_A_ENV"
    second_label="$ARM_B_LABEL"; second_kind="$ARM_B_KIND"; second_env="$ARM_B_ENV"
    order="${ARM_A_LABEL}_first"
  else
    first_label="$ARM_B_LABEL"; first_kind="$ARM_B_KIND"; first_env="$ARM_B_ENV"
    second_label="$ARM_A_LABEL"; second_kind="$ARM_A_KIND"; second_env="$ARM_A_ENV"
    order="${ARM_B_LABEL}_first"
  fi

  say "--- pair $i/$PAIRS ($order) ---"
  rid1="$(run_arm "$i" "$first_label" "$first_kind" "$first_env" 1)"
  if [[ -z "$rid1" ]]; then say "    pair $i ABANDONED (arm $first_label never produced a valid run)"; sleep 5; continue; fi
  sleep 5
  rid2="$(run_arm "$i" "$second_label" "$second_kind" "$second_env" 2)"
  if [[ -z "$rid2" ]]; then say "    pair $i ABANDONED (arm $second_label never produced a valid run)"; sleep 5; continue; fi

  # Normalise back to A/B regardless of execution order.
  if [[ "$first_label" == "$ARM_A_LABEL" ]]; then a_rid="$rid1"; b_rid="$rid2"
  else a_rid="$rid2"; b_rid="$rid1"; fi
  printf '%d\t%s\t%s\t%s\n' "$i" "$order" "$a_rid" "$b_rid" >> "$MANIFEST"
  ok_pairs=$((ok_pairs+1))
  say "    pair $i OK: A=$a_rid  B=$b_rid"
  sleep 5
done

say "=== paired run done: $ok_pairs/$PAIRS complete pairs; manifest $MANIFEST"
[[ $ok_pairs -gt 0 ]]
