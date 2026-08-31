#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 5 ]]; then
  echo "usage: $0 A2A_DATABASE_URL BINDING_ID HOST_ID BINDING_DIGEST PORTAL_DATABASE_URL" >&2
  echo "Both URLs must target disposable databases; the A2A database must use bundle 1.9.0." >&2
  exit 2
fi

fabric_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
workspace_root="$(cd -- "$fabric_root/.." && pwd)"

(
  cd "$fabric_root"
  A2A_STORE_TEST_DATABASE_URL="$1" \
  A2A_STORE_TEST_BINDING_ID="$2" \
  A2A_STORE_TEST_HOST_ID="$3" \
  A2A_STORE_TEST_BINDING_DIGEST="$4" \
  A2A_STORE_TEST_ENVIRONMENT=dev \
    cargo test -p a2a-store --test postgres_durability -- --nocapture
)

"$workspace_root/portal-db/postgres/tests/run-a2a-phase6-profiles-schema-gate.sh" "$5"

echo "A2A Phase 7 lease-takeover, dead-letter/restart, and Portal schema gates PASS"
