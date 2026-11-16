#!/usr/bin/env bash
# Rate-matched follow-ups at ~70% of Java's saturating req/s (production pods
# ran at ~71% of their cores). Rates are derived from the Java arms of the
# saturating batches on rust-03ddf2ab2, never chosen by hand.
set -u
H=/tmp/rust-proxy-qual/harness; L=/tmp/rust-proxy-qual/logs; R=/tmp/rust-proxy-qual/results
until grep -q PROD_DONE $L/prod-driver.log 2>/dev/null; do sleep 20; done
export RUST_PROXY_IMAGE=localhost:5001/migrations/capture_proxy:rust-03ddf2ab2 ARM_A_LABEL=java ARM_B_LABEL=rust
java_rate70() {  # manifest -> 70% of the Java arms' median req/s, integer
  python3 -I - "$1" <<'PY'
import json,statistics,sys
R='/tmp/rust-proxy-qual/results'
v=[json.load(open(f"{R}/{l.split()[2]}/summary.json"))['requests_per_s'] for l in open(sys.argv[1])]
print(int(round(0.70*statistics.median(v))))
PY
}
RP=$(java_rate70 $R/paired-gprod-prodshape-tls.manifest); echo "prodshape rate70=$RP"
ARM_A_ENV=$'MEASUREMENT_PATCH=1\nRATE_OVERRIDE='"$RP" ARM_B_ENV='RATE_OVERRIDE='"$RP" \
  "$H/run-paired.sh" gprodrate prodshape-tls 5 java rust 30 15
echo PRODRATE_DONE
RC=$(java_rate70 $R/paired-g4v4-plaintext-capture.manifest); echo "plaintext-capture rate70=$RC"
ARM_A_ENV=$'MEASUREMENT_PATCH=1\nRATE_OVERRIDE='"$RC" ARM_B_ENV='RATE_OVERRIDE='"$RC" \
  "$H/run-paired.sh" g4rate plaintext-capture 5 java rust 30 15
echo G4RATE_DONE
