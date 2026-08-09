#!/usr/bin/env bash
set -euo pipefail
export LIGHT_KNOWLEDGE_WORKER_CONFIG_FILE="${LIGHT_KNOWLEDGE_WORKER_CONFIG_FILE:-config/worker.yml}"
exec cargo run -p light-knowledge-worker -- "${1:-build}"
