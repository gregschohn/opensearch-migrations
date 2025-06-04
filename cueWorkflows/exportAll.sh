#!/bin/bash

set -x

cue export workflowTemplates/*.cue lib/*.cue lib/manifestHelpers/eval.cue allWorkflows.cue | jq -c '.documents[]'
