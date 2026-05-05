#!/bin/bash
set -e

if [[ -f /root/loadServicesFromParameterStore.sh ]]; then
  /root/loadServicesFromParameterStore.sh
fi

# Mount S3 bucket if configured via environment variables
if [[ -n "${S3_ARTIFACTS_BUCKET_NAME}" ]]; then
  MOUNT_POINT="/s3/artifacts"
  mkdir -p "$MOUNT_POINT"

  MOUNT_ARGS=("$S3_ARTIFACTS_BUCKET_NAME" "$MOUNT_POINT" "--read-only")
  [[ -n "${S3_ARTIFACTS_REGION}" ]] && MOUNT_ARGS+=("--region" "${S3_ARTIFACTS_REGION}")
  [[ -n "${S3_ARTIFACTS_ENDPOINT_URL}" ]] && MOUNT_ARGS+=("--endpoint-url" "${S3_ARTIFACTS_ENDPOINT_URL}" "--force-path-style" "--no-sign-request")

  echo "Waiting for S3 bucket ${S3_ARTIFACTS_BUCKET_NAME}..."
  until mount-s3 "${MOUNT_ARGS[@]}" 2>/dev/null; do
    echo "  bucket not available yet, retrying in 5s..."
    sleep 5
  done
  echo "Mounted ${S3_ARTIFACTS_BUCKET_NAME} at ${MOUNT_POINT}"
fi

echo "Console ready."
trap 'exit 0' TERM
sleep infinity &
wait $!
