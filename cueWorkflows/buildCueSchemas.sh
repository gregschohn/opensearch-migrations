#!/bin/bash

set -x

cd "$(dirname "$0")" || exit
BASE_BUILD=.build
mkdir -p $BASE_BUILD
TEMP_DIR=$(mktemp -d "${BASE_BUILD}/XXXXXX")
pushd $TEMP_DIR || exit

OUTPUT_DIR="../../cue.mod/pkg"
rm -rf ${OUTPUT_DIR:?}/*
K8S_OUTPUT_DIR="${OUTPUT_DIR}/k8s.io"
mkdir -p "$OUTPUT_DIR"

wget https://raw.githubusercontent.com/argoproj/argo-workflows/main/api/jsonschema/schema.json -O - | \
  jq 'del(.oneOf)' - | \
  cue import jsonschema: - -f -p argo -o "${OUTPUT_DIR}/github.com/opensearch-migrations/workflowconfigs/argo/argo.cue"


kubectl get --raw /openapi/v3 | \
  jq -r '.paths | to_entries[] | select(.key | startswith(".") | not) | select(.key | startswith("openid") | not) | .key + " " + .value.serverRelativeURL' | \
  while read -r path url; do
    # Extract the URL without the query part
    base_url=$(echo "$url" | cut -d'?' -f1)

    # Create a valid filename based on the path, without "apis_" prefix
    sanitized_path=${path//\//_}
    packagename=${sanitized_path//\./_}
    packagename=${packagename//-/_}
    # Remove "apis_" prefix if it exists
    basename="${sanitized_path#apis_}"

    kubectl get --raw "$base_url" | \
      cue import openapi: - -f -p "$packagename" -o "${K8S_OUTPUT_DIR}/${packagename}/${basename}.cue"
done

echo "Deleting temp directory, which should be empty"
trap 'popd && rmdir "$TEMP_DIR"' EXIT
