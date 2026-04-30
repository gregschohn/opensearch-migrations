#!/bin/bash

set -eo pipefail

SCRIPT_DIR="$(dirname "$0")"

echo "Stopping containers"
docker compose -f "$SCRIPT_DIR/docker-compose.yml" down
