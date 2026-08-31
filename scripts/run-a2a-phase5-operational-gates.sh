#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 4 ]]; then
  echo "usage: $0 A2A_DATABASE_URL BINDING_ID HOST_ID BINDING_DIGEST" >&2
  echo "The URL must target a disposable operations database bootstrapped with the A2A store migrations." >&2
  exit 2
fi

fabric_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

(
  cd "$fabric_root"
  A2A_STORE_TEST_DATABASE_URL="$1" \
  A2A_STORE_TEST_BINDING_ID="$2" \
  A2A_STORE_TEST_HOST_ID="$3" \
  A2A_STORE_TEST_BINDING_DIGEST="$4" \
  A2A_STORE_TEST_ENVIRONMENT=dev \
    cargo test -p a2a-store --test postgres_durability -- --nocapture
)

echo "A2A Phase 5 outbound ownership/replay durability gates PASS"
