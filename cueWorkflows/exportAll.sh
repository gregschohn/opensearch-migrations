#!/bin/bash

set -e

cue eval workflowTemplates/*.cue lib/*.cue lib/manifestHelpers/eval.cue allWorkflows.cue -c \
$(
  find workflowTemplates/scripts -type f | while read file; do
    key="resource_$(echo "${file#workflowTemplates/scripts/}" | tr '/.' '__')"
    val=$(base64 < "$file" | tr -d '\n')
    printf -- '--inject %s=%s ' "$key" "$val"
  done
) # | jq -c '.documents[]'
