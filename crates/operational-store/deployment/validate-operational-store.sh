#!/usr/bin/env bash
set -euo pipefail

database_name="${PORTAL_DB_OPERATIONAL_NAME:-operations}"
database_user="${POSTGRES_USER:-postgres}"
bundle_root="${OPERATIONAL_BUNDLE_ROOT:-/opt/operational-store/bundle}"
binding_id="${OPERATIONAL_BINDING_ID:-}"
binding_digest="${OPERATIONAL_BINDING_DIGEST:-}"
scope_id="${OPERATIONAL_SCOPE_ID:-}"
host_id="${OPERATIONAL_HOST_ID:-}"
environment_name="${OPERATIONAL_ENVIRONMENT:-}"
contract_generation="${OPERATIONAL_CONTRACT_GENERATION:-1}"

fail() {
  echo "operational-schema-validation: $*" >&2
  exit 1
}

[[ "$database_name" == "operations" ]] || fail "database identity must be operations"
[[ -f "$bundle_root/migration-order.tsv" ]] || fail "migration order is missing"

actual_binding="$(psql -U "$database_user" -d "$database_name" -X -tA -F '|' -c \
  "SELECT binding_id, binding_digest, scope_id, host_id, environment, database_identity, deployment_profile, schema_contract_generation, active FROM operational_meta.operational_store_binding_t WHERE active")"
expected_binding="$binding_id|$binding_digest|$scope_id|$host_id|$environment_name|operations|DEV_DEDICATED|$contract_generation|t"
[[ "$actual_binding" == "$expected_binding" ]] || fail "active scope root mismatch"

schema_ready="$(psql -U "$database_user" -d "$database_name" -X -tA <<'SQL'
SELECT current_database() = 'operations'
   AND to_regclass('operational_meta.operational_schema_migration_t') IS NOT NULL
   AND to_regclass('operational_meta.operational_store_binding_t') IS NOT NULL
   AND NOT EXISTS (
       SELECT required.schema_name
       FROM (VALUES
           ('operational_meta'), ('execution_ops'), ('agent_ops'), ('a2a_ops'),
           ('workflow_ops'), ('gateway_ops'), ('audit_ops'), ('artifact_ops')
       ) AS required(schema_name)
       WHERE to_regnamespace(required.schema_name) IS NULL
   );
SQL
)"
[[ "$schema_ready" == "t" ]] || fail "operational schema contract is incomplete"

expected_count=0
while IFS=$'\t' read -r order migration_owner schema_name migration_id migration_path migration_sha256; do
  [[ -n "$order" && "$order" != \#* ]] || continue
  expected_count=$((expected_count + 1))
  recorded_digest="$(psql -U "$database_user" -d "$database_name" -X -tAc \
    "SELECT migration_digest FROM operational_meta.operational_schema_migration_t WHERE migration_owner = '$migration_owner' AND schema_name = '$schema_name' AND migration_id = '$migration_id'")"
  [[ "$recorded_digest" == "sha256:$migration_sha256" ]] || fail "migration ledger mismatch for $migration_id"
done <"$bundle_root/migration-order.tsv"

actual_count="$(psql -U "$database_user" -d "$database_name" -X -tAc \
  "SELECT count(*) FROM operational_meta.operational_schema_migration_t")"
[[ "$actual_count" == "$expected_count" ]] || fail "unexpected migration ledger cardinality"

least_privilege_ready="$(psql -U "$database_user" -d "$database_name" -X -tA <<'SQL'
SELECT NOT rolsuper
   AND NOT rolcreatedb
   AND NOT rolcreaterole
   AND NOT has_database_privilege('operations_agent_runtime', 'operations', 'CREATE')
   AND NOT has_schema_privilege('operations_agent_runtime', 'agent_ops', 'CREATE')
   AND NOT has_schema_privilege('operations_agent_runtime', 'workflow_ops', 'USAGE')
FROM pg_roles
WHERE rolname = 'operations_agent_runtime';
SQL
)"
[[ "$least_privilege_ready" == "t" ]] || fail "runtime role privilege boundary is invalid"

execution_ready="$(psql -U "$database_user" -d "$database_name" -X -tA <<'SQL'
SELECT NOT rolsuper
   AND NOT rolcreatedb
   AND NOT rolcreaterole
   AND NOT has_database_privilege('operations_execution_runtime', 'operations', 'CREATE')
   AND NOT has_schema_privilege('operations_execution_runtime', 'execution_ops', 'CREATE')
   AND has_schema_privilege('operations_execution_runtime', 'execution_ops', 'USAGE')
   AND NOT EXISTS (
       SELECT required.table_name
       FROM unnest(ARRAY[
           'runner_session_t','runner_backend_t','runner_scheduling_request_t',
           'execution_session_t','execution_session_cleanup_request_t',
           'execution_attempt_t','execution_credential_grant_audit_t',
           'execution_fixed_action_t','execution_input_t','execution_provenance_t',
           'execution_runtime_audit_t','execution_runtime_tool_manifest_t'
       ]) AS required(table_name)
       WHERE to_regclass('execution_ops.' || required.table_name) IS NULL
          OR NOT has_table_privilege(
              'operations_execution_runtime',
              'execution_ops.' || required.table_name,
              'SELECT,INSERT,UPDATE,DELETE'
          )
   )
FROM pg_roles
WHERE rolname = 'operations_execution_runtime';
SQL
)"
[[ "$execution_ready" == "t" ]] || fail "execution runtime privilege or schema boundary is invalid"

agent_ready="$(psql -U "$database_user" -d "$database_name" -X -tA <<'SQL'
SELECT NOT rolsuper
   AND NOT rolcreatedb
   AND NOT rolcreaterole
   AND NOT has_database_privilege('operations_agent_runtime', 'operations', 'CREATE')
   AND NOT has_schema_privilege('operations_agent_runtime', 'agent_ops', 'CREATE')
   AND has_schema_privilege('operations_agent_runtime', 'agent_ops', 'USAGE')
   AND NOT EXISTS (
       SELECT required.table_name
       FROM unnest(ARRAY[
           'agent_action_attempt_t','agent_approval_t','agent_delegation_replay_t',
           'agent_fixed_action_t','agent_job_t','agent_memory_bank_t',
           'agent_memory_doc_t','agent_memory_entity_t','agent_memory_entity_cooccur_t',
           'agent_memory_link_t','agent_memory_reflection_t','agent_memory_unit_t',
           'agent_memory_unit_entity_t','agent_policy_snapshot_t',
           'agent_quota_reservation_t','agent_quota_usage_t','agent_session_event_t',
           'agent_session_history_t','agent_session_t','agent_turn_materialization_t',
           'agent_turn_t','agent_execution_outbox_t','runtime_operational_scope_t',
           'operational_reference_evidence_t','operational_reference_reconciliation_t'
       ]) AS required(table_name)
       WHERE to_regclass('agent_ops.' || required.table_name) IS NULL
          OR NOT has_table_privilege(
              'operations_agent_runtime',
              'agent_ops.' || required.table_name,
              'SELECT,INSERT,UPDATE,DELETE'
          )
   )
FROM pg_roles
WHERE rolname = 'operations_agent_runtime';
SQL
)"
[[ "$agent_ready" == "t" ]] || fail "Agent runtime privilege or schema boundary is invalid"

echo "Validated operations.operational_meta, execution_ops, and agent_ops for Host $host_id, environment $environment_name."
