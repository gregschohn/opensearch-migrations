#!/bin/sh
# Generates the capture proxy's self-signed TLS certificate for the docker-compose dev stacks.
#
# The Java proxy image had a self-signed certificate and private key baked in at build time
# (trafficCaptureProxyServer's generateSelfSignedCerts), so every copy of that image shared one key.
# The Rust image ships no TLS material, and the proxy refuses to start (exit 70) when the files its
# command line names are missing. docker-compose.yml and docker-compose.large-requests.yml therefore
# run this once per stack in an init container: it writes pub.pem and key.pem into a volume that the
# proxy mounts read-only at /usr/share/captureProxy/config, the path the Java image used, so the
# proxy's command line and every client of https://capture-proxy:9200 stay unchanged.
#
# Development only. The certificate is self-signed; clients in these stacks already connect with
# certificate verification disabled (services.yaml allow_insecure, curl -k).
#
# Usage: capture-proxy-dev-tls.sh [output-directory]   (default /tls)
set -eu

out="${1:-/tls}"
# The proxy image runs as this unprivileged user (TrafficCapture/captureProxy/Dockerfile).
proxy_uid="${CAPTURE_PROXY_UID:-10001}"

if [ -s "${out}/pub.pem" ] && [ -s "${out}/key.pem" ]; then
  echo "capture-proxy-dev-tls: ${out}/pub.pem and ${out}/key.pem already exist; keeping them"
  exit 0
fi

if ! command -v openssl >/dev/null 2>&1; then
  # The compose files run this in the stack's Kafka image (Alpine), which carries openssl. The
  # Amazon Linux 2023 base image does not.
  echo "capture-proxy-dev-tls: openssl is not installed in this image" >&2
  exit 1
fi

umask 077
mkdir -p "${out}"
openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 3650 \
  -subj "/CN=capture-proxy" \
  -addext "subjectAltName=DNS:capture-proxy,DNS:localhost,IP:127.0.0.1" \
  -keyout "${out}/key.pem.tmp" -out "${out}/pub.pem.tmp" 2>/dev/null
chown "${proxy_uid}:${proxy_uid}" "${out}/key.pem.tmp" "${out}/pub.pem.tmp"
chmod 0400 "${out}/key.pem.tmp"
chmod 0444 "${out}/pub.pem.tmp"
mv "${out}/key.pem.tmp" "${out}/key.pem"
mv "${out}/pub.pem.tmp" "${out}/pub.pem"
echo "capture-proxy-dev-tls: wrote a self-signed certificate for capture-proxy to ${out}"
openssl x509 -in "${out}/pub.pem" -noout -subject -enddate
