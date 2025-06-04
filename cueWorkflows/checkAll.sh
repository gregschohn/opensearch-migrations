#!/bin/bash

set -x

cue vet workflowTemplates/*.cue lib/*cue lib/manifestHelpers/check.cue -c 2>& 1
