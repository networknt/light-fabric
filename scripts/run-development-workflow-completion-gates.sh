#!/usr/bin/env bash
set -euo pipefail
# Explicit disposable databases only. The store gate refuses an existing schema;
# Controller's fixture owns only its runner-postgres-test origin and runner.
: "${DEVELOPMENT_WORKFLOW_TEST_DATABASE_URL:?fresh disposable Workflow database required}"
: "${EXECUTION_DATABASE_URL:?disposable database with canonical execution schema required}"
: "${EXECUTION_HOST_ID:?disposable execution Host required}"
root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
bash scripts/run-development-workflow-store-gate.sh
cargo test --locked -p light-workflow --test snapshot_transfer
cargo test --locked -p task-workspace --test workspaces fixed_manager_snapshot_read
cargo check --locked -p light-workflow -p light-agent -p light-agent-worker --all-targets
cd "$root/../controller-rs"
cargo test --locked --test runner_postgres -- --nocapture
git diff --check
echo 'Component/database gates passed; authenticated native and installer application qualification are separate.'
