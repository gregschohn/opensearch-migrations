#!/usr/bin/env bash
# Profile definitions. Sourced by run-profile.sh.
#
# Each profile fixes one axis of the comparison matrix. Dimensions:
#   FRONT_TLS   proxy terminates client TLS
#   BACK_TLS    proxy opens TLS to the source
#   CAPTURE     1 = capture to Kafka, 0 = --noCapture (pure scan-and-forward)
#   CHURN       fraction of requests on a brand-new connection
#   FRAG_PROB   fraction of requests split across several TCP segments
#   CONNS       generator concurrency (keep-alive workers)
#   RATE        offered requests/s; 0 = unthrottled (saturating)
#   KAFKA_PATH  direct | toxi
#   KAFKA_DELAY_MS  latency toxic applied on the Kafka path
#
# Saturating load (RATE=0) is the default because the continuation gates are
# stated per USED CORE. At a fixed sub-saturation rate both implementations
# would do the same work and the ratio would mostly reflect idle-loop cost; at
# saturation the ratio reflects the cost of the work itself. Latency numbers
# from a saturating run are latency AT SATURATION at a fixed concurrency, and
# are only comparable between Java and Rust at that same concurrency.

load_profile() {
  local p="$1"
  FRONT_TLS=0; BACK_TLS=0; CAPTURE=1; CHURN=0.0; FRAG_PROB=0.10
  CONNS=64; RATE=0; KAFKA_PATH=direct; KAFKA_DELAY_MS=0
  MIX="bulk_post=70,get=18,put=4,delete=2,patch=2,chunked_trailers=4"

  case "$p" in
    # Gate 2: plaintext scan-and-forward throughput per used core.
    plaintext-nocapture)
      CAPTURE=0 ;;

    # Gate 4: thin end-to-end capture slice, healthy Kafka.
    plaintext-capture)
      : ;;

    front-tls)
      FRONT_TLS=1 ;;

    # Gate 3 (one half): double TLS.
    double-tls)
      FRONT_TLS=1; BACK_TLS=1 ;;

    # Keep-alive heavy: zero churn, no fragmentation, high concurrency. This is
    # the cheapest-per-request shape, so it isolates steady-state decode cost.
    keepalive-heavy)
      CHURN=0.0; FRAG_PROB=0.0; CONNS=192 ;;

    # Gate 3 (other half): ~23% new connections on TLS, matching the
    # production report's "about 23% of requests open a new TLS connection".
    churn23-tls)
      FRONT_TLS=1; CHURN=0.23 ;;

    # Production shape: TLS on both sides AND ~23% new connections, capture
    # on, healthy Kafka. Neither double-tls (no churn) nor churn23-tls (no
    # back TLS) combines them; this is the shape the gate-3 wording and the
    # final F2 target refer to.
    prodshape-tls)
      FRONT_TLS=1; BACK_TLS=1; CHURN=0.23 ;;

    # Control for kafka-delayed: identical extra network hop, no toxic. Without
    # this control, the delayed-Kafka numbers would also carry the cost of the
    # extra proxy hop and could not be attributed to the delay.
    kafka-healthy)
      KAFKA_PATH=toxi; KAFKA_DELAY_MS=0 ;;

    # Gate 6: delayed Kafka acknowledgements. 750 ms is chosen to sit well
    # inside the producer's 10 s delivery timeout (the production report notes
    # produce requests outlasting that timeout stop the proxy) so that
    # backpressure, not termination, is what is being measured.
    kafka-delayed)
      KAFKA_PATH=toxi; KAFKA_DELAY_MS=750 ;;

    *) echo "unknown profile: $p" >&2; return 1 ;;
  esac

  # Calibration overrides. Used to find the concurrency at which the proxy
  # becomes CPU-bound rather than in-flight-limited; the chosen value is then
  # baked into the profile so Java and Rust are compared at the same
  # concurrency.
  CONNS="${CONNS_OVERRIDE:-$CONNS}"
  RATE="${RATE_OVERRIDE:-$RATE}"
  CHURN="${CHURN_OVERRIDE:-$CHURN}"
  FRAG_PROB="${FRAG_OVERRIDE:-$FRAG_PROB}"
  return 0
}

ALL_PROFILES=(
  plaintext-nocapture
  plaintext-capture
  front-tls
  double-tls
  keepalive-heavy
  churn23-tls
  kafka-healthy
  kafka-delayed
)
