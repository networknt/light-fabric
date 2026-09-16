#!/usr/bin/env bash
set -euo pipefail

# This is the stage-store slice, NOT the full Phase 1 runtime qualification.
# Supply a fresh disposable PostgreSQL database. The test refuses an existing
# workflow_ops schema and never drops data or silently skips PostgreSQL.
if [[ -z "${DEVELOPMENT_WORKFLOW_TEST_DATABASE_URL:-}" ]]; then
  echo "Set DEVELOPMENT_WORKFLOW_TEST_DATABASE_URL to a fresh disposable database." >&2
  exit 2
fi
repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_dir"
cargo test --locked -p development-workflow-contract
cargo test --locked -p light-workflow --test development_store_postgres -- --ignored --nocapture
cargo test --locked -p light-workflow --lib
git diff --check
echo "Development stage-store checks passed; full Phase 1 qualification remains required."
