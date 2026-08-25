#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
workspace_root=$(cd "$repo_root/.." && pwd)

cd "$repo_root"
./scripts/run-light-workflow-config-controller-phase3-gate.sh

cargo test --locked -p light-runtime reload_context_fetches_remote_values_into_external_cache
cargo test --locked -p light-runtime config_server_rejects_an_advertised_digest_that_does_not_match_the_body
cargo test --locked -p light-runtime critical_failure_permanently_prevents_admission_reopen
cargo test --locked -p light-runtime startup_fails_if_a_critical_participant_failed_before_admission_opens
cargo test --locked -p light-runtime targeted_reload_does_not_mutate_unrequested_runtime_modules
cargo test --locked -p light-runtime reload_all_is_rejected_before_any_module_mutates_when_target_is_exclusive
cargo test --locked -p light-runtime runtime_mcp_handler_serializes_overlapping_reload_operations
cargo test --locked -p light-workflow workflow_runtime_generation_swaps_one_complete_policy
cargo test --locked -p light-workflow unchanged_policy_refreshes_provenance_without_advancing_generation
cargo test --locked -p light-workflow capacity_consumers_are_notified_of_the_committed_generation
cargo test --locked -p light-workflow effective_value_digest_is_independent_of_input_map_order
cargo test --locked -p light-workflow reload_lkg_rejects_source_bytes_that_do_not_match_candidate_provenance
cargo test --locked -p light-workflow rejected_configuration_keeps_the_active_generation_ready
cargo test --locked -p light-workflow workflow_reloader_is_exclusive_and_rejects_without_mutating_active_generation
cargo test --locked -p light-workflow embedded_and_remote_values_build_the_same_typed_candidate
cargo test --locked -p light-workflow remote_boot_then_lkg_outage_recovery_is_identity_bound
cargo test --locked -p light-workflow invocation_scope_token_expiry_is_enforced_at_startup_but_not_reaged_on_reload
cargo test --locked -p light-workflow unexpected_task_failure_marks_readiness_and_cancels_siblings
cargo check --locked -p light-workflow --all-targets
bash -n apps/light-workflow/run.sh

if rg -q 'WORKFLOW_INVOCATION_CALLER_SERVICE_IDS|LIGHT_WORKFLOW_HTTP_ADDR' \
  apps/light-workflow/run.sh; then
  echo "Phase 4 launcher still references removed workflow environment variables." >&2
  exit 1
fi
rg -Fq 'LIGHT_WORKFLOW_CONFIG_MODE' apps/light-workflow/run.sh

for marker in \
  'WORKFLOW_RUNTIME_MODULE_ID' \
  'WorkflowConfigManager' \
  'WorkflowRestartBaseline' \
  'requires_exclusive_reload' \
  'source_values_yaml' \
  'validate_remote_reload(' \
  'persist_remote_reload(' \
  'restart_required_differences(' \
  '.run_dynamic(executor_runtime_config, shutdown)'; do
  if ! rg -Fq "$marker" apps/light-workflow/src/main.rs apps/light-workflow/src/configuration.rs; then
    echo "Phase 4 atomic workflow reload marker is missing: $marker" >&2
    exit 1
  fi
done

for metric in \
  light_workflow_config_active_info \
  light_workflow_config_refresh_total \
  light_workflow_config_candidate_rejections_total \
  light_workflow_config_lkg_uses_total \
  light_workflow_config_last_success_unixtime_seconds \
  light_workflow_registry_connected \
  light_workflow_lifecycle_drain_state \
  light_workflow_capacity_configured; do
  if ! rg -Fq "$metric" apps/light-workflow/src/rule_api.rs; then
    echo "Phase 4 workflow metric is not emitted: $metric" >&2
    exit 1
  fi
done

if ! rg -Fq 'source: currentPromotedSnapshot' \
  apps/light-workflow/config-contract/reload-lifecycle.yml; then
  echo "Phase 4 current-snapshot-only refresh contract is missing." >&2
  exit 1
fi

for deployment in \
  "$workspace_root/portal-config-dev" \
  "$workspace_root/portal-config-loc/all-in-lt" \
  "$workspace_root/light-portal-install"; do
  publisher="$deployment/light-workflow-rust/publish-current-snapshot.sh"
  rollback="$deployment/light-workflow-rust/rollback-current-snapshot.sh"
  bash -n "$publisher" "$rollback"
  test -x "$rollback"
  for marker in \
    'previous snapshot' \
    'target_snapshot' \
    "service_id = 'com.networknt.workflow-1.0.0'" \
    'UPDATE config_snapshot_t' \
    'light-workflow/runtime-config'; do
    if ! rg -Fq "$marker" "$publisher" "$rollback"; then
      echo "Phase 4 snapshot rollback marker is missing in $deployment: $marker" >&2
      exit 1
    fi
  done
done

if [[ -n "${PORTAL_DB_PHASE4_TEST_URL:-}" ]]; then
  phase4_schema="light_workflow_phase4_$$"
  phase4_instance="01a00000-0000-7000-8000-000000000401"
  phase4_host="01a00000-0000-7000-8000-000000000402"
  phase4_snapshot_a="01a00000-0000-7000-8000-000000000403"
  phase4_snapshot_b="01a00000-0000-7000-8000-000000000404"
  cleanup_phase4_schema() {
    psql "$PORTAL_DB_PHASE4_TEST_URL" -v ON_ERROR_STOP=1 \
      -c "DROP SCHEMA IF EXISTS $phase4_schema CASCADE" >/dev/null
  }
  trap cleanup_phase4_schema EXIT
  psql "$PORTAL_DB_PHASE4_TEST_URL" -v ON_ERROR_STOP=1 <<SQL
CREATE SCHEMA $phase4_schema;
CREATE TABLE $phase4_schema.instance_t (
  instance_id uuid PRIMARY KEY,
  host_id uuid NOT NULL,
  service_id varchar NOT NULL,
  active boolean NOT NULL
);
CREATE TABLE $phase4_schema.config_snapshot_t (
  snapshot_id uuid PRIMARY KEY,
  host_id uuid NOT NULL,
  instance_id uuid NOT NULL,
  service_id varchar NOT NULL,
  current boolean NOT NULL
);
INSERT INTO $phase4_schema.instance_t VALUES
  ('$phase4_instance', '$phase4_host', 'com.networknt.workflow-1.0.0', true);
INSERT INTO $phase4_schema.config_snapshot_t VALUES
  ('$phase4_snapshot_a', '$phase4_host', '$phase4_instance', 'com.networknt.workflow-1.0.0', true),
  ('$phase4_snapshot_b', '$phase4_host', '$phase4_instance', 'com.networknt.workflow-1.0.0', false);
SQL
  rollback_script="$workspace_root/portal-config-dev/light-workflow-rust/rollback-current-snapshot.sh"
  PGOPTIONS="-c search_path=$phase4_schema" \
    LIGHT_WORKFLOW_DATABASE_URL="$PORTAL_DB_PHASE4_TEST_URL" \
    LIGHT_WORKFLOW_CONFIG_INSTANCE_ID="$phase4_instance" \
    LIGHT_WORKFLOW_SNAPSHOT_DRY_RUN=true \
    "$rollback_script" "$phase4_snapshot_b" >/dev/null
  current_snapshot=$(PGOPTIONS="-c search_path=$phase4_schema" \
    psql "$PORTAL_DB_PHASE4_TEST_URL" -Atqc \
      "SELECT snapshot_id FROM config_snapshot_t WHERE current = true")
  [[ "$current_snapshot" == "$phase4_snapshot_a" ]]

  PGOPTIONS="-c search_path=$phase4_schema" \
    LIGHT_WORKFLOW_DATABASE_URL="$PORTAL_DB_PHASE4_TEST_URL" \
    LIGHT_WORKFLOW_CONFIG_INSTANCE_ID="$phase4_instance" \
    "$rollback_script" "$phase4_snapshot_b" >/dev/null
  current_snapshot=$(PGOPTIONS="-c search_path=$phase4_schema" \
    psql "$PORTAL_DB_PHASE4_TEST_URL" -Atqc \
      "SELECT snapshot_id FROM config_snapshot_t WHERE current = true")
  [[ "$current_snapshot" == "$phase4_snapshot_b" ]]
  cleanup_phase4_schema
  trap - EXIT
else
  echo "PORTAL_DB_PHASE4_TEST_URL is unset; skipping disposable rollback transaction gate."
fi

docker compose -f "$workspace_root/portal-config-dev/docker-compose.yml" config --quiet
docker compose \
  -f "$workspace_root/portal-config-loc/all-in-lt/docker-compose.yml" \
  -f "$workspace_root/portal-config-loc/all-in-lt/docker-compose-rust.yml" \
  config --quiet
docker compose -f "$workspace_root/light-portal-install/docker-compose.yml" config --quiet

echo "Light Workflow Config Server/controller Phase 4 source gate passed."
echo "Loc and dev live qualification remains an explicit deployment gate."
