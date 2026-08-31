#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 AGENT_DATABASE_URL HOST_ID" >&2
  echo "The URL must use operations_agent_runtime against a disposable operations database bootstrapped with bundle 1.6.0." >&2
  exit 2
fi

fabric_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

(
  cd "$fabric_root"
  AGENT_STORE_TEST_DATABASE_URL="$1" \
  AGENT_STORE_TEST_HOST_ID="$2" \
    cargo test -p agent-store --test native_a2a_postgres -- --nocapture
)

echo "A2A Phase 4 operational durability gates PASS"
