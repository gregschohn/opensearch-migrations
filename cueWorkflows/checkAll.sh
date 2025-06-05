#!/bin/bash

set -x

cue vet workflowTemplates/*.cue lib/*.cue lib/manifestHelpers/eval.cue -c 2>& 1 \
$(
  find workflowTemplates/scripts -type f | while read file; do
    key="resource_$(echo "${file#workflowTemplates/scripts/}" | tr '/.' '__')"
    val=$(base64 < "$file" | tr -d '\n')
    printf -- '--inject %s=%s ' "$key" "$val"
  done
) | jq -c '.documents[]'