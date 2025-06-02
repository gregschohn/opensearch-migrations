#!/bin/bash

set -x

for FILE in workflowTemplates/*.cue; do
  cue vet ${FILE} lib/*cue lib/manifestHelpers/check.cue -c 2>& 1
done
