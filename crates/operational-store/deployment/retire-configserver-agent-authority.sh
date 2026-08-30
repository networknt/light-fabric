#!/usr/bin/env bash
set -euo pipefail

database_user="${POSTGRES_USER:-postgres}"
configserver_database="${PORTAL_DB_NAME:-configserver}"
configserver_schema="${PORTAL_DB_CONFIGSERVER_SCHEMA:-public}"
runtime_role="${CONFIGSERVER_RUNTIME_ROLE:-}"

fail() {
  echo "agent-source-retirement: $*" >&2
  exit 1
}

[[ "$configserver_database" == "configserver" ]] || fail "source database must be configserver"
[[ "$configserver_schema" =~ ^[a-z_][a-z0-9_]*$ ]] || fail "invalid Config Server schema"
[[ "$runtime_role" =~ ^[a-z_][a-z0-9_]*$ ]] || fail "invalid or missing Config Server runtime role"

destination_ready="$(psql -U "$database_user" -d operations -X -tAc \
  "SELECT EXISTS(SELECT 1 FROM operational_meta.operational_schema_migration_t WHERE migration_owner='agent-store' AND schema_name='agent_ops' AND migration_id='0001_agent_and_embedded_memory')")"
[[ "$destination_ready" == "t" ]] || fail "Agent destination is not authoritative"

role_exists="$(psql -U "$database_user" -d "$configserver_database" -X -tAc \
  "SELECT EXISTS(SELECT 1 FROM pg_roles WHERE rolname='${runtime_role}')")"
[[ "$role_exists" == "t" ]] || fail "Config Server runtime role does not exist"

tables=(
  agent_action_attempt_t agent_approval_t agent_delegation_replay_t
  agent_fixed_action_t agent_job_t agent_memory_bank_t agent_memory_doc_t
  agent_memory_entity_t agent_memory_entity_cooccur_t agent_memory_link_t
  agent_memory_reflection_t agent_memory_unit_t agent_memory_unit_entity_t
  agent_policy_snapshot_t agent_quota_reservation_t agent_quota_usage_t
  agent_session_event_t agent_session_history_t agent_session_t
  agent_turn_materialization_t agent_turn_t agent_execution_outbox_t
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

echo "Retired Config Server Agent writes for role $runtime_role; source rows were not copied."
