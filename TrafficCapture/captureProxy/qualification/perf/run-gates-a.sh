#!/usr/bin/env bash
# Gate 2 then Gate 4 (+ pinned-heap corroboration). Java arms use the
# measurement overlay so every Java arm is launched by the identical mechanism.
set -u
H=/tmp/rust-proxy-qual/harness
export ARM_A_LABEL=java ARM_B_LABEL=rust
ARM_A_ENV=$'MEASUREMENT_PATCH=1' ARM_B_ENV=$'' \
  "$H/run-paired.sh" g2 plaintext-nocapture 10 java rust 30 15
ARM_A_ENV=$'MEASUREMENT_PATCH=1' ARM_B_ENV=$'' \
  "$H/run-paired.sh" g4 plaintext-capture 10 java rust 30 15
ARM_A_LABEL=javapin ARM_A_ENV=$'MEASUREMENT_PATCH=1\nHEAP_OPTS=-Xms536m -Xmx536m' ARM_B_ENV=$'' \
  "$H/run-paired.sh" g4pin plaintext-capture 5 java rust 30 15
echo GATES_A_DONE
