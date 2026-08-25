#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
postgres_url="${1:-}"

cd "$repo_dir"

cargo fmt --all --check
cargo test --locked -p light-workflow --test config_server_controller_phase0
cargo test --locked -p light-workflow --bin light-workflow
cargo test --locked -p light-workflow --lib rule_api::tests
cargo test --locked -p light-workflow --test example_workflows

if [[ -n "$postgres_url" ]]; then
  DATABASE_URL="$postgres_url" cargo test --locked -p light-workflow --test postgres_runner_integration \
    terminal_result_survives_origin_restart_and_is_accepted_once
  DATABASE_URL="$postgres_url" cargo test --locked -p light-workflow --test postgres_runner_integration \
    runner_origin_scope_changes_scheduling_idempotency_namespace
  DATABASE_URL="$postgres_url" cargo test --locked -p light-workflow --test postgres_runner_integration \
    concurrent_scheduler_claims_create_one_fenced_attempt
  DATABASE_URL="$postgres_url" cargo test --locked -p light-workflow --test postgres_runner_integration \
    stale_terminal_result_cannot_cross_a_newer_fencing_token
else
  echo "Phase 0 PostgreSQL characterization skipped; pass a disposable PostgreSQL URL to include it."
fi

echo "Light Workflow Config Server/controller Phase 0 contracts and characterization gates passed."
