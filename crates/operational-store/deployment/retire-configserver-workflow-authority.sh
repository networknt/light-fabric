#!/usr/bin/env bash
set -euo pipefail

database_user="${POSTGRES_USER:-postgres}"
configserver_database="${PORTAL_DB_NAME:-configserver}"
configserver_schema="${PORTAL_DB_CONFIGSERVER_SCHEMA:-public}"
runtime_role="${CONFIGSERVER_RUNTIME_ROLE:-}"

fail() {
  echo "workflow-source-retirement: $*" >&2
  exit 1
}

[[ "$configserver_database" == "configserver" ]] || fail "source database must be configserver"
[[ "$configserver_schema" =~ ^[a-z_][a-z0-9_]*$ ]] || fail "invalid Config Server schema"
[[ "$runtime_role" =~ ^[a-z_][a-z0-9_]*$ ]] || fail "invalid or missing Config Server runtime role"

destination_ready="$(psql -U "$database_user" -d operations -X -tAc \
  "SELECT EXISTS(SELECT 1 FROM operational_meta.operational_schema_migration_t WHERE migration_owner='workflow-store' AND schema_name='workflow_ops' AND migration_id='0001_workflow_runtime')")"
[[ "$destination_ready" == "t" ]] || fail "Workflow destination is not authoritative"

tables=(
  process_info_t task_info_t workflow_approval_t workflow_artifact_t
  workflow_executor_tenant_turn_t workflow_fork_branch_t workflow_fork_join_t
  workflow_invocation_audit_outbox_t workflow_invocation_budget_reservation_t
  workflow_invocation_budget_t workflow_invocation_event_quarantine_t
  workflow_invocation_idempotency_t workflow_invocation_t workflow_task_effect_t
  workflow_tool_access_request_item_t workflow_tool_access_request_t
  workflow_tool_approval_evidence_t
)
for table_name in "${tables[@]}"; do
  exists="$(psql -U "$database_user" -d "$configserver_database" -X -tAc \
    "SELECT to_regclass('${configserver_schema}.${table_name}') IS NOT NULL")"
  [[ "$exists" == "t" ]] || fail "source table is missing: $table_name"
  psql -U "$database_user" -d "$configserver_database" -X --set=ON_ERROR_STOP=1 \
    --set=source_schema="$configserver_schema" --set=source_table="$table_name" \
    --set=runtime_role="$runtime_role" <<'SQL' >/dev/null
REVOKE INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER
    ON TABLE :"source_schema".:"source_table" FROM :"runtime_role";
SQL
  writable="$(psql -U "$database_user" -d "$configserver_database" -X -tAc \
    "SELECT has_table_privilege('${runtime_role}','${configserver_schema}.${table_name}','INSERT') OR has_table_privilege('${runtime_role}','${configserver_schema}.${table_name}','UPDATE') OR has_table_privilege('${runtime_role}','${configserver_schema}.${table_name}','DELETE')")"
  [[ "$writable" == "f" ]] || fail "source write authority remains on $table_name"
done

echo "Retired Config Server Workflow operational writes for role $runtime_role; projection authoring and source rows were not changed."
