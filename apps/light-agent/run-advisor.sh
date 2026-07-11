#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

DEFAULT_SERVICE_ID='com.networknt.agent.advisor-1.0.0'
DEFAULT_LIGHT_AGENT_HOST_ID='01964b05-552a-7c4b-9184-6857e7f3dc5f'

TOKEN="${1:-${LIGHT_PORTAL_AUTHORIZATION:-}}"
SERVICE_ID="${2:-${LIGHT_AGENT_SERVICE_ID:-$DEFAULT_SERVICE_ID}}"
HOST_ID="${3:-${LIGHT_AGENT_HOST_ID:-$DEFAULT_LIGHT_AGENT_HOST_ID}}"

if [[ -z "${TOKEN//[[:space:]]/}" ]]; then
  echo "A bearer token is required as argument 1 or LIGHT_PORTAL_AUTHORIZATION." >&2
  exit 2
fi

export LIGHT_PORTAL_AUTHORIZATION="$TOKEN"
export SERVER_SERVICEID="$SERVICE_ID"
export LIGHT_AGENT_HOST_ID="$HOST_ID"

cd "$SCRIPT_DIR"
cargo run
