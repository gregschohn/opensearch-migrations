#!/bin/bash

set -e -x # Exit on any error

# Check if config filename argument is provided
if [ $# -eq 0 ]; then
    echo "Error: CONFIG_FILENAME argument is required"
    echo "Usage: $0 <config-filename> [additional-args...]"
    exit 1
fi

CONFIG_FILENAME=$1
shift  # Remove first argument, leaving any additional args in $@

# Get the directory where this script is located
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
: "${NODEJS:=node}"

# Default command, can be overridden by setting INITIALIZE_CMD environment variable
: ${INITIALIZE_CMD:="$NODEJS $SCRIPT_DIR/index.js initialize"}

# Create a temporary directory for output files
TEMP_DIR=$(mktemp -d)

# Ensure cleanup on exit
trap "rm -rf $TEMP_DIR" EXIT

UUID="$(date +%s)"
echo "Generated unique uniqueRunNonce: $UUID"

WORKFLOW_NAME="migration-workflow"

echo "Running configuration conversion..."
$INITIALIZE_CMD --user-config $CONFIG_FILENAME --output-dir $TEMP_DIR --workflow-name "$WORKFLOW_NAME" $@

echo "Applying Kubernetes resources..."
if [ -x "$TEMP_DIR/handleK8sResources.sh" ]; then
    "$TEMP_DIR/handleK8sResources.sh"
fi

if [ -x "$TEMP_DIR/enrichWorkflowConfigWithUids.sh" ]; then
    echo "Enriching workflow config with CR UIDs..."
    "$TEMP_DIR/enrichWorkflowConfigWithUids.sh" "$TEMP_DIR/workflowMigration.config.yaml"
fi

echo "Applying workflow to Kubernetes..."

# Display any initialization warnings
if [ -f "$TEMP_DIR/warnings.json" ]; then
    "$NODEJS" -e "JSON.parse(require('fs').readFileSync('$TEMP_DIR/warnings.json','utf8')).forEach(w=>console.log('INIT_WARNING: '+w))" >&2
fi

cat <<EOF | kubectl create -f -
apiVersion: argoproj.io/v1alpha1
kind: Workflow
metadata:
  name: $WORKFLOW_NAME
spec:
  workflowTemplateRef:
    name: full-migration
  entrypoint: main
  arguments:
    parameters:
      - name: uniqueRunNonce
        value: "$UUID"
      - name: approval-config
        value: "approval-config-0"
      - name: config
        value: |
$(sed 's/^/          /' "$TEMP_DIR/workflowMigration.config.yaml")
EOF

echo "Done! Workflow submitted successfully."
