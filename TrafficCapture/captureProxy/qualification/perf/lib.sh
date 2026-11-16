#!/usr/bin/env bash
# Shared definitions for the capture-proxy qualification harness.
#
# CORE BUDGET (Docker Desktop Linux VM: 18 CPUs, ~43 GiB)
#
# Every workload is pinned with --cpuset-cpus to a disjoint core range. The
# point is that the proxy's cpu.stat usage_usec must contain ONLY the proxy's
# work: if the generator, the source, Kafka or the sampler shared a core with
# the proxy, the proxy's scheduling delay would change its own cache behaviour
# and its throughput-per-core number would be a property of the harness rather
# than of the proxy.
#
#   0-4   proxy under test      (5 cores, --memory=5000m: the production pod shape)
#   5-7   HTTP source           (3 cores)
#   8-10  Kafka broker          (3 cores)
#  11-15  load generator        (5 cores)
#  16-17  toxiproxy, OTel collector, cgroup sampler (2 cores)
set -euo pipefail

CORES_PROXY="${CORES_PROXY:-0-4}"
CORES_SOURCE="${CORES_SOURCE:-5-7}"
CORES_KAFKA="${CORES_KAFKA:-8-10}"
CORES_GEN="${CORES_GEN:-11-15}"
CORES_INFRA="${CORES_INFRA:-16-17}"

# Production per-pod shape, from docs/captureAndReplay/loadTestRun-2026-10-13-160k.md
PROXY_MEM="${PROXY_MEM:-5000m}"
PROXY_CORES_N=5

HARNESS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="${ROOT:-/tmp/rust-proxy-qual}"
RESULTS="$ROOT/results"
LOGS="$ROOT/logs"
PROFILES_DIR="$ROOT/profiles"
CERTS="$ROOT/certs"

JAVA_PROXY_IMAGE="${JAVA_PROXY_IMAGE:-localhost:5001/migrations/capture_proxy:latest_arm64}"
RUST_PROXY_IMAGE="${RUST_PROXY_IMAGE:-localhost:5001/migrations/capture_proxy:rust-748379d05}"
KAFKA_IMAGE="${KAFKA_IMAGE:-apache/kafka:3.9.1}"
TOXI_IMAGE="${TOXI_IMAGE:-ghcr.io/shopify/toxiproxy:2.12.0}"
GEN_IMAGE="${GEN_IMAGE:-qualgen:1}"
ALPINE_IMAGE="${ALPINE_IMAGE:-alpine:3.20}"
OTEL_IMAGE="${OTEL_IMAGE:-otel/opentelemetry-collector-contrib:0.111.0}"

SAMPLE_INTERVAL="${SAMPLE_INTERVAL:-1}"

log()  { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*" >&2; }
die()  { printf '[%s] FATAL: %s\n' "$(date +%H:%M:%S)" "$*" >&2; exit 1; }

host_loadavg() { uptime | sed 's/.*load averages*: *//' | tr -s ' ' | tr ' ' ','; }

vm_loadavg() {
  docker run --rm --cpuset-cpus="$CORES_INFRA" "$ALPINE_IMAGE" \
    sh -c 'cut -d" " -f1-3 /proc/loadavg' 2>/dev/null | tr ' ' ',' || echo "NA"
}

# Generate the self-signed TLS material the proxy and the source serve.
# Separate CAs for front and back so a double-TLS run really does two
# independent handshakes rather than resuming one session.
ensure_certs() {
  mkdir -p "$CERTS"
  for who in proxy source; do
    if [[ ! -s "$CERTS/$who-key.pem" ]]; then
      openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
        -subj "/CN=localhost" \
        -addext "subjectAltName=DNS:localhost,DNS:$who,DNS:qual-source,IP:127.0.0.1" \
        -keyout "$CERTS/$who-key.pem" -out "$CERTS/$who-cert.pem" >/dev/null 2>&1 \
        || die "openssl failed generating $who cert"
      chmod 644 "$CERTS/$who-key.pem" "$CERTS/$who-cert.pem"
    fi
  done
}

# Wait until a TCP port inside the docker network accepts a connection.
#
# The poll loop runs INSIDE one short-lived container rather than starting a
# container per attempt. The earlier per-attempt version could start up to 90
# containers while waiting for Kafka, and Docker Desktop's container-create
# path intermittently stalls for many minutes under load — which corrupted the
# time base of two runs before this was changed. One container per wait keeps
# container creations per run in the single digits.
wait_tcp() {
  local netname="$1" hostport="$2" tries="${3:-60}"
  local h="${hostport%%:*}" p="${hostport##*:}"
  docker run --rm --network "$netname" --cpuset-cpus="$CORES_INFRA" \
    "$ALPINE_IMAGE" sh -c \
    "i=0; while [ \$i -lt $tries ]; do nc -z -w2 $h $p && exit 0; sleep 1; i=\$((i+1)); done; exit 1" \
    >/dev/null 2>&1
}

cleanup_run() {
  local run="$1"
  docker rm -f "px-$run" "src-$run" "kafka-$run" "toxi-$run" "gen-$run" \
                "smp-$run" "otel-$run" >/dev/null 2>&1 || true
  docker network rm "net-$run" >/dev/null 2>&1 || true
}
