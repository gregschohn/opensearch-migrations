#!/usr/bin/env bash
#   run-g5v4.sh <rust-image>
# Gate 5 v4: (a) 10 saturating pairs, (b) 10 rate-matched pairs at 15K req/s
# for BOTH arms (neither saturates; broker sees equal offered load),
# (c) 3 confirmation pairs each for gates 2 and 4.
set -u
H=/tmp/rust-proxy-qual/harness; L=/tmp/rust-proxy-qual/logs; R=/tmp/rust-proxy-qual/results
export RUST_PROXY_IMAGE="${1:?rust image required}"
export ARM_A_LABEL=java ARM_B_LABEL=rust
docker image inspect "$RUST_PROXY_IMAGE" >/dev/null 2>&1 || { echo NO_IMAGE; exit 1; }
# Probe: ack instrument present on the gate bucket set, and kafkaCommitCount
# must no longer exceed records accepted (the double-count defect).
"$H/run-profile.sh" probe-v4 rust plaintext-capture 15 10 > $L/probe-v4.log 2>&1
python3 -I - "$R/probe-v4" <<'PY' || { echo PROBE_FAIL; exit 2; }
import json,re,sys
d=sys.argv[1]; t=open(d+'/otel-metrics.prom').read()
def tot(n):
    return sum(float(m.group(1)) for m in re.finditer(r'^'+n+r'\{([^}]*)\}\s+([0-9eE.+-]+)', t, re.M) for _ in [0] if True) if False else \
      sum(float(x) for x in re.findall(r'^'+n+r'\{[^}]*\}\s+([0-9eE.+-]+)', t, re.M))
cc=tot('kafkaCommitCount_total')
tr=sum(float(x) for x in re.findall(r'^capturePublishingRecordsAccepted_records_total\{[^}]*recordKind="traffic"[^}]*\}\s+([0-9eE.+-]+)', t, re.M))
le=re.findall(r'^kafkaCommitDuration_milliseconds_bucket\{[^}]*le="([^"]+)"', t, re.M)
print(f"kafkaCommitCount={cc:.0f} trafficRecordsAccepted={tr:.0f} ratio={cc/tr if tr else -1:.3f} buckets={len(le)}")
ok = tr>0 and 0.9 <= cc/tr <= 1.1 and len(le)==18
sys.exit(0 if ok else 1)
PY
echo PROBE_OK
J=$'MEASUREMENT_PATCH=1'
ARM_A_ENV="$J" ARM_B_ENV='' "$H/run-paired.sh" g5v4 kafka-healthy 10 java rust 30 15
ARM_A_ENV=$'MEASUREMENT_PATCH=1\nRATE_OVERRIDE=15000' ARM_B_ENV=$'RATE_OVERRIDE=15000' \
  "$H/run-paired.sh" g5v4rate kafka-healthy 10 java rust 30 15
ARM_A_ENV="$J" ARM_B_ENV='' "$H/run-paired.sh" g2v4 plaintext-nocapture 3 java rust 30 15
ARM_A_ENV="$J" ARM_B_ENV='' "$H/run-paired.sh" g4v4 plaintext-capture 3 java rust 30 15
echo G5V4_DONE
