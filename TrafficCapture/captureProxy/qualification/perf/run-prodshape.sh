#!/usr/bin/env bash
set -u
H=/tmp/rust-proxy-qual/harness; L=/tmp/rust-proxy-qual/logs
until grep -q DIAG_DONE $L/diag-driver.log 2>/dev/null; do sleep 20; done
export RUST_PROXY_IMAGE=localhost:5001/migrations/capture_proxy:rust-03ddf2ab2 ARM_A_LABEL=java ARM_B_LABEL=rust
ARM_A_ENV=$'MEASUREMENT_PATCH=1' ARM_B_ENV='' "$H/run-paired.sh" gprod prodshape-tls 5 java rust 30 15
echo PROD_DONE
