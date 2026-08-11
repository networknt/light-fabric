#!/usr/bin/env bash
set -euo pipefail
export LIGHT_KNOWLEDGE_CONFIG_DIR="${LIGHT_KNOWLEDGE_CONFIG_DIR:-config}"
exec cargo run -p light-knowledge-worker -- "${1:-build-loop}"
