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
FLAGS=""
if [ "$MODE" = "eval" ]; then
    FLAGS="-c"
elif [ "$MODE" != "export" ] && [ "$MODE" != "vet" ]; then
    usage
    exit 2
fi

cd "$(dirname "$0")" || exit 3

cue ${MODE} `find lib workflowTemplates -name \*.cue` allWorkflows.cue $FLAGS 2>& 1 \
$(
  find workflowTemplates/scripts -type f | while read file; do
    key="resource_$(echo "${file#workflowTemplates/scripts/}" | tr '/.' '__')"
    val=$(base64 < "$file" | tr -d '\n')
    printf -- '--inject %s=%s ' "$key" "$val"
  done
) -t ${MODE}
