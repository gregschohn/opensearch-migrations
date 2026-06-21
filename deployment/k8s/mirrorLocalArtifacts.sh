#!/usr/bin/env bash

set -euo pipefail

MIGRATIONS_REPO_ROOT_DIR=$(git rev-parse --show-toplevel)

source "${MIGRATIONS_REPO_ROOT_DIR}/buildImages/backends/dockerHostedBuildkit.sh"
source "${MIGRATIONS_REPO_ROOT_DIR}/deployment/k8s/charts/aggregates/migrationAssistantWithArgo/scripts/mirrorToEcr.sh"
source "${MIGRATIONS_REPO_ROOT_DIR}/deployment/k8s/charts/aggregates/testClusters/testClustersEcrManifest.sh"

usage() {
  cat <<'EOF'
Usage: deployment/k8s/mirrorLocalArtifacts.sh [options]

Mirrors third-party chart and image dependencies into the local Docker registry
used by kind/minikube development clusters.

Options:
  --registry HOST              Host-side registry endpoint to push to.
                               Default: localhost:${EXTERNAL_REGISTRY_PORT:-5001}
  --manifest FILE              Migration Assistant mirror manifest.
                               Default: chart infra/mirror/private-ecr-manifest.yaml
  --all                        Mirror the full manifest instead of the local-dev subset.
  --images-only                Mirror images only.
  --charts-only                Mirror charts only.
  --help                       Show this help.

Environment:
  MIRROR_MAX_JOBS              Concurrent image copies. Default: 4.
  LOCAL_MIRROR_PLATFORM        Image platform to mirror. Default: linux/<host arch>.
  LOCAL_CRANE_COPY_JOBS        Concurrent layer copies per image. Default: 2.
  LOCAL_REGISTRY_PLAIN_HTTP    Use plain HTTP for chart pushes. Default: true.
EOF
}

manifest_file="${MIGRATIONS_REPO_ROOT_DIR}/deployment/k8s/charts/aggregates/migrationAssistantWithArgo/infra/mirror/private-ecr-manifest.yaml"
registry=""
profile="local-dev"
mirror_images=true
mirror_charts=true

while [[ $# -gt 0 ]]; do
  case "$1" in
    --registry)
      registry="$2"; shift 2 ;;
    --manifest)
      manifest_file="$2"; shift 2 ;;
    --all)
      profile="all"; shift ;;
    --images-only)
      mirror_charts=false; shift ;;
    --charts-only)
      mirror_images=false; shift ;;
    --help|-h)
      usage; exit 0 ;;
    *)
      echo "Unknown option: $1" >&2
      usage >&2
      exit 1 ;;
  esac
done

set_docker_hosted_defaults
registry="${registry:-localhost:${EXTERNAL_REGISTRY_PORT}}"
export KUBE_CONTEXT="${KUBE_CONTEXT:-local-artifact-mirror}"

echo "Preparing local registry service..."
setup_build_backend

load_private_ecr_manifest "$manifest_file"

local_chart_names='^(cert-manager|strimzi-kafka-operator|argo-workflows|fluent-bit|kube-prometheus-stack|localstack|kyverno)$'
local_image_patterns='/(jetstack/cert-manager|strimzi/(operator:|kafka:)|argoproj/|fluent/fluent-bit|prometheus|kiwigrid/k8s-sidecar|kube-state-metrics|kube-webhook-certgen|thanos/thanos|grafana/grafana|bats/bats|localstack/localstack|amazon/aws-cli|aws-observability/aws-otel-collector|kyverno/|kyverno/readiness-checker|kubectl:|library/busybox:|opensearchproject/opensearch:)'

if [[ "$profile" == "local-dev" ]]; then
  CHARTS="$(printf '%s\n' "$CHARTS" | awk -F'|' -v names="$local_chart_names" '$1 ~ names')"
  IMAGES="$(printf '%s\n' "$IMAGES" | grep -E "$local_image_patterns" || true)"
fi

CHARTS="$(printf '%s\n%s\n' "$CHARTS" "$TEST_CHARTS" | awk 'NF && !seen[$0]++')"
IMAGES="$(printf '%s\n%s\n' "$IMAGES" "$TEST_IMAGES" | awk 'NF && !seen[$0]++')"

echo "Mirroring profile: ${profile}"
echo "Registry: ${registry}"

if [[ "$mirror_images" == "true" ]]; then
  mirror_images_to_registry "$registry" "$IMAGES"
fi

if [[ "$mirror_charts" == "true" ]]; then
  mirror_charts_to_registry "$registry" "$CHARTS"
fi

cat <<EOF

Local artifact mirror is ready.

Use this values override for in-cluster chart/image pulls:
  deployment/k8s/charts/aggregates/migrationAssistantWithArgo/scripts/generateLocalRegistryValues.sh docker-registry:${EXTERNAL_REGISTRY_PORT}
EOF
