#!/bin/bash

set -e

usage() {
    echo "Usage: $0 <vet|eval|export>"
}

if [ $# -eq 0 ]; then
    usage
    exit 1
fi

MODE=$1
shift
CUE=~/cue_v0.14.0-alpha.1_darwin_arm64/cue
FLAGS=""
if [ "$MODE" = "eval" ]; then
    FLAGS=""
elif [ "$MODE" != "export" ] && [ "$MODE" != "vet" ]; then
    usage
    exit 2
fi

cd "$(dirname "$0")"/.. || exit 3

$CUE ${MODE} `find lib workflowTemplates -name \*.cue | grep -v tests` allWorkflows.cue $FLAGS 2>& 1 \
$(
  find workflowTemplates/scripts -type f | while read file; do
    key="resource_$(echo "${file#workflowTemplates/scripts/}" | tr '/.' '__')"
    val=$(base64 < "$file" | tr -d '\n')
    printf -- '--inject %s=%s ' "$key" "$val"
  done
) -t ${MODE} "$@"
