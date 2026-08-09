#!/usr/bin/env bash
set -euo pipefail
export LIGHT_KNOWLEDGE_CONFIG_FILE="${LIGHT_KNOWLEDGE_CONFIG_FILE:-config/knowledge.yml}"
exec cargo run -p light-knowledge
