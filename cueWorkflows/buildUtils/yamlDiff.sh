#!/bin/bash

# Script to compare two YAML files by converting them to JSON with sorted keys
# Usage: ./compare_yaml.sh file1.yaml file2.yaml

# Check if two arguments were provided
if [ $# -ne 2 ]; then
  echo "Usage: $0 <yaml_file1> <yaml_file2>"
  echo "Example: $0 out/proxy.yaml ../TrafficCapture/dockerSolution/src/main/docker/migrationConsole/workflows/templates/proxy.yaml"
  exit 1
fi

# Check if files exist
if [ ! -f "$1" ]; then
  echo "Error: File '$1' not found"
  exit 1
fi

if [ ! -f "$2" ]; then
  echo "Error: File '$2' not found"
  exit 1
fi

# Function to convert YAML to JSON with sorted keys
yaml_to_json() {
  python3 -c "import yaml, json, sys; print(json.dumps(yaml.safe_load(open('$1')), sort_keys=True, indent=2))"
}

# Perform the diff
diff -u <(yaml_to_json "$1") <(yaml_to_json "$2") | less