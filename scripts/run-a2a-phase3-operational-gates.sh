#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 6 ]]; then
  echo "usage: $0 A2A_DATABASE_URL ARTIFACT_DATABASE_URL BINDING_ID HOST_ID BINDING_DIGEST ENVIRONMENT" >&2
  echo "Both URLs must target the same empty, disposable operations database." >&2
  exit 2
fi

fabric_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

(
  cd "$fabric_root"
  A2A_STORE_TEST_DATABASE_URL="$1" \
  A2A_STORE_TEST_BINDING_ID="$3" \
  A2A_STORE_TEST_HOST_ID="$4" \
  A2A_STORE_TEST_BINDING_DIGEST="$5" \
  A2A_STORE_TEST_ENVIRONMENT="$6" \
    cargo test -p a2a-store --test postgres_durability -- --nocapture

  ARTIFACT_STORE_TEST_DATABASE_URL="$2" \
  PHASE6_TEST_BINDING_ID="$3" \
  PHASE6_TEST_HOST_ID="$4" \
  PHASE6_TEST_BINDING_DIGEST="$5" \
  PHASE6_TEST_ENVIRONMENT="$6" \
    cargo test -p artifact-store --test postgres_artifact -- --nocapture
)

echo "A2A Phase 3 operational durability gates PASS"
