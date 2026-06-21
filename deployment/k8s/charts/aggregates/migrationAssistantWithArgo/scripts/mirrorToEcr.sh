#!/bin/bash
# =============================================================================
# mirrorToEcr.sh
#
# Copies all required container images and helm charts from public registries
# to a private ECR registry. Run this from a machine with internet access.
#
# Can be run standalone or sourced by the migration-assistant CLI (which calls
# mirror_images_to_ecr and mirror_charts_to_ecr directly).
#
# Usage: ./mirrorToEcr.sh <ecr-host> [--region <region>]
# Example: ./mirrorToEcr.sh 123456789012.dkr.ecr.us-east-2.amazonaws.com
# =============================================================================
set -euo pipefail

default_private_ecr_manifest() {
  local script_dir
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  echo "$(cd "$script_dir/../infra/mirror" && pwd)/private-ecr-manifest.yaml"
}

manifest_section() {
  local manifest="$1" section="$2"
  awk -v section="$section" '
    /^[A-Za-z0-9_-]+:[[:space:]]*$/ {
      if (in_section) {
        exit
      }
      in_section = ($1 == section ":")
      next
    }
    in_section {
      original = $0
      sub(/^[[:space:]]*-[[:space:]]*/, "", $0)
      if ($0 == original) {
        next
      }
      sub(/[[:space:]]*#.*$/, "", $0)
      gsub(/^[[:space:]]+|[[:space:]]+$/, "", $0)
      if ($0 == "") {
        next
      }
      if ($0 ~ /^".*"$/ || $0 ~ /^'\''.*'\''$/) {
        $0 = substr($0, 2, length($0) - 2)
      }
      print
    }
  ' "$manifest"
}

load_private_ecr_manifest() {
  local manifest="${1:-}"
  if [ -z "$manifest" ]; then
    manifest="$(default_private_ecr_manifest)"
  fi
  [ -f "$manifest" ] || { echo "Missing ECR mirror manifest: $manifest" >&2; return 1; }

  CHARTS="$(manifest_section "$manifest" charts)"
  IMAGES="$(manifest_section "$manifest" images)"

  [ -n "$CHARTS" ] || { echo "No charts found in ECR mirror manifest: $manifest" >&2; return 1; }
  [ -n "$IMAGES" ] || { echo "No images found in ECR mirror manifest: $manifest" >&2; return 1; }
}

# --- install crane if missing ---
ensure_crane() {
  command -v crane >/dev/null 2>&1 && return
  echo "Installing crane..."
  local arch os crane_dir
  arch=$(uname -m)
  case "$arch" in x86_64|amd64) arch="x86_64" ;; aarch64|arm64) arch="arm64" ;; esac
  os=$(uname -s)
  crane_dir="${HOME}/bin"
  mkdir -p "$crane_dir"
  curl -sL "https://github.com/google/go-containerregistry/releases/latest/download/go-containerregistry_${os}_${arch}.tar.gz" \
    | tar xz -C "$crane_dir" crane
  export PATH="${crane_dir}:${PATH}"
  echo "crane installed to ${crane_dir}/crane"
}

wait_for_copy_slot() {
  local max_jobs="$1"
  while [ "$(jobs -rp | wc -l)" -ge "$max_jobs" ]; do
    sleep 1
  done
}

crane_copy_with_timeout() {
  local src="$1" dest="$2"
  local platform="${3:-}"
  local copy_jobs="${4:-}"
  local timeout_seconds="${CRANE_COPY_TIMEOUT_SECONDS:-300}"
  local copy_pid watchdog_pid status
  local args=(copy)

  if [ -n "$copy_jobs" ]; then
    args+=(--jobs "$copy_jobs")
  fi
  if [ -n "$platform" ]; then
    args+=(--platform "$platform")
  fi
  crane "${args[@]}" "$src" "$dest" &
  copy_pid=$!
  (
    sleep "$timeout_seconds"
    kill "$copy_pid" >/dev/null 2>&1 || true
  ) &
  watchdog_pid=$!

  if wait "$copy_pid"; then
    status=0
  else
    status=$?
  fi

  kill "$watchdog_pid" >/dev/null 2>&1 || true
  wait "$watchdog_pid" 2>/dev/null || true
  return "$status"
}

# Registry hostname → ECR pull-through cache prefix (matches PullThroughCacheHelper.groovy)
# Args: <ptc-endpoint> <image>
ptc_rewrite() {
  local ptc="$1" image="$2"
  [ -z "$ptc" ] && return 1
  local path prefix
  case "$image" in
    docker.io/*)            prefix="docker-hub";          path="${image#docker.io/}" ;;
    registry-1.docker.io/*) prefix="docker-hub";          path="${image#registry-1.docker.io/}" ;;
    mirror.gcr.io/*)        prefix="docker-hub";          path="${image#mirror.gcr.io/}" ;;
    public.ecr.aws/*)       prefix="ecr-public";          path="${image#public.ecr.aws/}" ;;
    ghcr.io/*)              prefix="github-container-registry"; path="${image#ghcr.io/}" ;;
    registry.k8s.io/*)      prefix="k8s";                 path="${image#registry.k8s.io/}" ;;
    quay.io/*)              prefix="quay";                path="${image#quay.io/}" ;;
    *) return 1 ;;
  esac
  echo "${ptc}/${prefix}/${path}"
}

image_mirror_destination() {
  local registry="$1" image="$2"
  local image_no_tag="${image%%:*}" tag="${image##*:}"
  echo "${registry}/mirrored/${image_no_tag}:${tag}"
}

image_source_candidates() {
  local image="$1" dockerhub_mirrors="$2"
  local sources="$image"
  case "$image" in mirror.gcr.io/*)
    local path="${image#mirror.gcr.io/}"
    sources=""
    for m in $dockerhub_mirrors; do
      if [ "$m" = "public.ecr.aws" ]; then
        case "$path" in library/*) sources="${sources:+$sources }${m}/docker/${path}" ;; esac
      else
        sources="${sources:+$sources }${m}/${path}"
      fi
    done
  ;; esac
  echo "$sources"
}

image_exists_in_registry() {
  local image="$1"
  crane digest "$image" >/dev/null 2>&1
}

local_registry_platform() {
  if [ -n "${LOCAL_MIRROR_PLATFORM:-}" ]; then
    echo "$LOCAL_MIRROR_PLATFORM"
    return
  fi

  local arch
  arch="$(uname -m)"
  case "$arch" in
    x86_64|amd64) echo "linux/amd64" ;;
    aarch64|arm64) echo "linux/arm64" ;;
    *) echo "Unsupported local mirror architecture: $arch" >&2; return 1 ;;
  esac
}

# Copy a single image to ECR, trying pull-through cache then mirror sources.
# Args: <ecr-host> <region> <dockerhub-mirrors> <ptc-endpoint> <image>
copy_image() {
  local ecr_host="$1" region="$2" dockerhub_mirrors="$3" ptc="$4" image="$5"
  local image_no_tag="${image%%:*}"
  local ecr_repo="mirrored/${image_no_tag}"
  local dest
  dest="$(image_mirror_destination "$ecr_host" "$image")"
  local sources
  sources="$(image_source_candidates "$image" "$dockerhub_mirrors")"

  aws ecr create-repository --repository-name "$ecr_repo" --region "$region" 2>/dev/null || true

  # Prepend pull-through cache source if available (fastest path — same-region ECR to ECR)
  local ptc_src
  ptc_src=$(ptc_rewrite "$ptc" "$image") && sources="$ptc_src ${sources}"

  for src in $sources; do
    for attempt in 1 2 3 4 5; do
      if crane_copy_with_timeout "$src" "$dest"; then
        [ "$src" != "$image" ] && echo "  ℹ️  $image (via $src)"
        echo "  ✅ $image"; return 0
      fi
      sleep $((5 * 2**(attempt-1)))  # exponential backoff: 5s, 10s, 20s, 40s, 80s
    done
  done
  echo "  ❌ $image" >&2; return 1
}

# Copy a single image to any registry using the same mirrored path convention
# as ECR, but without AWS authentication or repository creation.
# Args: <registry-host> <dockerhub-mirrors> <ptc-endpoint> <image>
copy_image_to_registry() {
  local registry="$1" dockerhub_mirrors="$2" ptc="$3" image="$4"
  local dest
  dest="$(image_mirror_destination "$registry" "$image")"
  local sources
  sources="$(image_source_candidates "$image" "$dockerhub_mirrors")"
  local platform
  platform="$(local_registry_platform)"
  local copy_jobs="${LOCAL_CRANE_COPY_JOBS:-${CRANE_COPY_JOBS:-2}}"

  if image_exists_in_registry "$dest"; then
    echo "  ✅ $image (already mirrored)"; return 0
  fi

  local ptc_src
  ptc_src=$(ptc_rewrite "$ptc" "$image") && sources="$ptc_src ${sources}"

  for src in $sources; do
    if crane_copy_with_timeout "$src" "$dest" "$platform" "$copy_jobs"; then
      [ "$src" != "$image" ] && echo "  ℹ️  $image (via $src)"
      echo "  ✅ $image"; return 0
    fi
    if image_exists_in_registry "$dest"; then
      [ "$src" != "$image" ] && echo "  ℹ️  $image (via $src)"
      echo "  ✅ $image (verified after copy)"; return 0
    fi
  done
  echo "  ❌ $image" >&2; return 1
}

# Mirror container images to ECR (parallel).
# Args: <ecr-host> <region> <images-string>
mirror_images_to_ecr() {
  local ecr_host="$1" region="$2" images="$3"
  local dockerhub_mirrors="${DOCKERHUB_MIRRORS:-mirror.gcr.io docker.io public.ecr.aws}"

  ensure_crane

  echo "Authenticating with ECR ($ecr_host)..."
  local ecr_pass
  ecr_pass=$(aws ecr get-login-password --region "$region")
  echo "$ecr_pass" | crane auth login "$ecr_host" -u AWS --password-stdin

  aws ecr-public get-login-password --region us-east-1 2>/dev/null | \
    crane auth login public.ecr.aws -u AWS --password-stdin 2>/dev/null || true

  # ECR pull-through cache (optional, from Jenkins host environment)
  local ptc="${ECR_PULL_THROUGH_ENDPOINT:-}"
  if [ -n "$ptc" ]; then
    echo "ECR pull-through cache enabled: $ptc"
    aws ecr get-login-password --region "$region" 2>/dev/null | \
      crane auth login "$ptc" -u AWS --password-stdin 2>/dev/null || true
  fi

  echo ""
  echo "=== Mirroring container images ==="
  local _imglist _max_jobs=4 _failfile
  _imglist=$(mktemp)
  _failfile=$(mktemp)
  echo "$images" | sed 's/^[[:space:]]*//' | sed 's/[[:space:]]*$//' | grep -v '^#' | grep -v '^$' > "$_imglist"
  while IFS= read -r image; do
    # Throttle to $_max_jobs concurrent copies
    wait_for_copy_slot "$_max_jobs"
    ( copy_image "$ecr_host" "$region" "$dockerhub_mirrors" "$ptc" "$image" || echo "$image" >> "$_failfile" ) &
  done < "$_imglist"
  rm -f "$_imglist"
  wait
  if [ -s "$_failfile" ]; then
    echo "" >&2
    echo "❌ The following image copies failed:" >&2
    sed 's/^/  - /' "$_failfile" >&2
    echo "" >&2
    echo "If this is unexpected, please open an issue at:" >&2
    echo "  https://github.com/opensearch-project/opensearch-migrations/issues/new" >&2
    rm -f "$_failfile"
    exit 1
  fi
  rm -f "$_failfile"
}

# Mirror container images to a generic registry such as a local Docker registry.
# Args: <registry-host> <images-string>
mirror_images_to_registry() {
  local registry="$1" images="$2"
  local dockerhub_mirrors="${DOCKERHUB_MIRRORS:-mirror.gcr.io docker.io public.ecr.aws}"

  ensure_crane

  local ptc="${LOCAL_PULL_THROUGH_ENDPOINT:-${ECR_PULL_THROUGH_ENDPOINT:-}}"
  if [ -n "$ptc" ]; then
    echo "Pull-through cache source enabled: $ptc"
  fi

  echo ""
  echo "=== Mirroring container images to ${registry} ==="
  local _imglist _max_jobs="${MIRROR_MAX_JOBS:-4}" _failfile
  _imglist=$(mktemp)
  _failfile=$(mktemp)
  echo "$images" | sed 's/^[[:space:]]*//' | sed 's/[[:space:]]*$//' | grep -v '^#' | grep -v '^$' > "$_imglist"
  while IFS= read -r image; do
    wait_for_copy_slot "$_max_jobs"
    ( copy_image_to_registry "$registry" "$dockerhub_mirrors" "$ptc" "$image" || echo "$image" >> "$_failfile" ) &
  done < "$_imglist"
  rm -f "$_imglist"
  wait
  if [ -s "$_failfile" ]; then
    echo "" >&2
    echo "❌ The following image copies failed:" >&2
    sed 's/^/  - /' "$_failfile" >&2
    rm -f "$_failfile"
    exit 1
  fi
  rm -f "$_failfile"
}

# Mirror helm charts to ECR as OCI artifacts.
# Args: <ecr-host> <region> <charts-string>
mirror_charts_to_ecr() {
  local ecr_host="$1" region="$2" charts="$3"

  ensure_crane

  local ecr_pass
  ecr_pass=$(aws ecr get-login-password --region "$region")
  echo "$ecr_pass" | helm registry login "$ecr_host" -u AWS --password-stdin

  # Log helm into public.ecr.aws so pulls of OCI charts hosted there
  # (e.g. aws-controllers-k8s/acmpca-chart) don't stall on anonymous
  # rate-limits / credential prompts. Best-effort — anonymous pulls
  # usually work, but authenticated pulls are more reliable.
  aws ecr-public get-login-password --region us-east-1 2>/dev/null | \
    helm registry login public.ecr.aws -u AWS --password-stdin 2>/dev/null || true

  echo ""
  echo "=== Mirroring Helm charts ==="
  local _chartlist _chart_fail=0
  _chartlist=$(mktemp)
  echo "$charts" | grep -v '^[[:space:]]*$' > "$_chartlist"
  while IFS='|' read -r name version repo; do
    name="${name#"${name%%[![:space:]]*}"}"; name="${name%"${name##*[![:space:]]}"}"
    version="${version#"${version%%[![:space:]]*}"}"; version="${version%"${version##*[![:space:]]}"}"
    repo="${repo#"${repo%%[![:space:]]*}"}"; repo="${repo%"${repo##*[![:space:]]}"}"
    [ -z "$name" ] && continue

    echo "  Pulling $name $version from $repo..."
    # Retry transient pull failures (DNS, github.io 5xx, OCI registry blip)
    # the way image pulls already do. stdin from /dev/null so a credential
    # prompt fails fast instead of hanging the whole bootstrap.
    local _pull_err _attempt _ok=0
    _pull_err=$(mktemp)
    for _attempt in 1 2 3 4 5; do
      if echo "$repo" | grep -q '^oci://'; then
        helm pull "$repo/$name" --version "$version" </dev/null 2>"$_pull_err" && { _ok=1; break; }
      else
        helm pull "$name" --repo "$repo" --version "$version" </dev/null 2>"$_pull_err" && { _ok=1; break; }
      fi
      [ "$_attempt" -lt 5 ] && sleep $((_attempt * 5))
    done
    if [ "$_ok" -ne 1 ]; then
      echo "  ❌ FAILED to pull $name $version after 5 attempts" >&2
      sed 's/^/    /' "$_pull_err" >&2
      rm -f "$_pull_err"
      continue
    fi
    rm -f "$_pull_err"

    local tgz
    tgz=$(ls ${name}-*.tgz 2>/dev/null | head -1)
    if [ -z "$tgz" ]; then
      echo "  ❌ FAILED to download $name $version" >&2
      continue
    fi

    aws ecr create-repository --repository-name "charts/${name}" --region "$region" 2>/dev/null || true
    echo "  Pushing $tgz → oci://${ecr_host}/charts"
    helm push "$tgz" "oci://${ecr_host}/charts" </dev/null 2>&1 || { echo "  ❌ FAILED to push $name" >&2; _chart_fail=1; }
    rm -f "$tgz"
    echo "  ✅ $name $version"
  done < "$_chartlist"
  rm -f "$_chartlist"
  [ "$_chart_fail" -eq 0 ] || echo "⚠️  Some chart copies failed" >&2

  echo ""
  echo "=== Mirroring complete ==="
}

# Mirror helm charts to a generic OCI registry. For a local Docker registry this
# usually needs --plain-http because the registry is intentionally not TLS-backed.
# Args: <registry-host> <charts-string>
mirror_charts_to_registry() {
  local registry="$1" charts="$2"
  local plain_http="${LOCAL_REGISTRY_PLAIN_HTTP:-true}"
  local push_flags=()
  if [ "$plain_http" = "true" ]; then
    push_flags+=(--plain-http)
  fi

  ensure_crane

  echo ""
  echo "=== Mirroring Helm charts to ${registry} ==="
  local _chartlist _chart_fail=0 _workdir
  _chartlist=$(mktemp)
  _workdir=$(mktemp -d)
  echo "$charts" | grep -v '^[[:space:]]*$' > "$_chartlist"
  if (
    cd "$_workdir"
    while IFS='|' read -r name version repo; do
      name="${name#"${name%%[![:space:]]*}"}"; name="${name%"${name##*[![:space:]]}"}"
      version="${version#"${version%%[![:space:]]*}"}"; version="${version%"${version##*[![:space:]]}"}"
      repo="${repo#"${repo%%[![:space:]]*}"}"; repo="${repo%"${repo##*[![:space:]]}"}"
      [ -z "$name" ] && continue

      echo "  Pulling $name $version from $repo..."
      local _pull_err _ok=0
      _pull_err=$(mktemp)
      if echo "$repo" | grep -q '^oci://'; then
        helm pull "$repo/$name" --version "$version" </dev/null 2>"$_pull_err" && _ok=1
      else
        helm pull "$name" --repo "$repo" --version "$version" </dev/null 2>"$_pull_err" && _ok=1
      fi
      if [ "$_ok" -ne 1 ]; then
        echo "  ❌ FAILED to pull $name $version" >&2
        sed 's/^/    /' "$_pull_err" >&2
        rm -f "$_pull_err"
        _chart_fail=1
        continue
      fi
      rm -f "$_pull_err"

      local tgz
      tgz=$(ls "${name}"-*.tgz 2>/dev/null | head -1)
      if [ -z "$tgz" ]; then
        echo "  ❌ FAILED to download $name $version" >&2
        _chart_fail=1
        continue
      fi

      echo "  Pushing $tgz → oci://${registry}/charts"
      if helm push "$tgz" "oci://${registry}/charts" "${push_flags[@]}" </dev/null 2>&1; then
        echo "  ✅ $name $version"
      else
        echo "  ❌ FAILED to push $name" >&2
        _chart_fail=1
      fi
      rm -f "$tgz"
    done < "$_chartlist"
    exit "$_chart_fail"
  ); then
    _chart_fail=0
  else
    _chart_fail=$?
  fi
  rm -f "$_chartlist"
  rm -rf "$_workdir"
  [ "$_chart_fail" -eq 0 ] || exit "$_chart_fail"

  echo ""
  echo "=== Mirroring complete ==="
}

# --- CLI entrypoint (only when run directly, not sourced) ---
if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

  ECR_HOST="${1:-}"
  shift || true
  REGION="${AWS_CFN_REGION:-us-east-1}"
  MANIFEST_FILE=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --region) REGION="$2"; shift 2 ;;
      --manifest) MANIFEST_FILE="$2"; shift 2 ;;
      *) shift ;;
    esac
  done

  if [ -z "$ECR_HOST" ]; then
    echo "Usage: $0 <ecr-host> [--region <region>]" >&2
    exit 1
  fi

  load_private_ecr_manifest "${MANIFEST_FILE:-}"
  mirror_images_to_ecr "$ECR_HOST" "$REGION" "$IMAGES"
  mirror_charts_to_ecr "$ECR_HOST" "$REGION" "$CHARTS"
fi
