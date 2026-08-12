#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
portal_db_dir="${PORTAL_DB_DIR:-$repo_dir/../portal-db}"
postgres_url="${1:-}"

cd "$repo_dir"

cargo fmt --all --check
cargo test --locked -p workflow-invocation-contract
cargo test --locked -p light-workflow --lib invocation::
cargo clippy --locked -p workflow-invocation-contract --all-targets --no-deps -- -D warnings

if [[ -n "$postgres_url" ]]; then
  "$portal_db_dir/postgres/tests/run-workflow-mcp-phase0-schema-gate.sh" "$postgres_url"
else
  echo "Phase 0 PostgreSQL runtime gate skipped; pass a disposable PostgreSQL URL to include it."
fi

echo "Workflow-backed MCP Phase 0 implementation checks passed; runtime promotion remains disabled pending live qualification."
