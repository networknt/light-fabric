#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
portal_db_dir="${PORTAL_DB_DIR:-$repo_dir/../portal-db}"
postgres_url="${1:-}"

cd "$repo_dir"

cargo fmt --all --check
cargo test --locked -p workflow-invocation-contract
cargo test --locked -p light-rule workflow_cel_value
cargo test --locked -p light-workflow --lib invocation::
cargo test --locked -p light-workflow --lib rule_api::
cargo test --locked -p light-workflow --lib executor::tests::interactive_host_execution
cargo test --locked -p light-pingora --lib workflow
cargo clippy --locked -p workflow-invocation-contract -p light-rule --all-targets --no-deps -- -D warnings
cargo clippy --locked -p light-workflow -p light-pingora -p light-gateway --all-targets --no-deps

if [[ -n "$postgres_url" ]]; then
  "$portal_db_dir/postgres/tests/run-workflow-mcp-phase1-schema-gate.sh" "$postgres_url"
else
  echo "Phase 1 PostgreSQL runtime gate skipped; pass a disposable PostgreSQL URL to include it."
fi

echo "Workflow-backed MCP Phase 1 implementation checks passed; this is not live workflow qualification."
