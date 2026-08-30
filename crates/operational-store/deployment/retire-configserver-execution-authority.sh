#!/usr/bin/env bash
set -euo pipefail

database_user="${POSTGRES_USER:-postgres}"
configserver_database="${PORTAL_DB_NAME:-configserver}"
configserver_schema="${PORTAL_DB_CONFIGSERVER_SCHEMA:-public}"
runtime_role="${CONFIGSERVER_RUNTIME_ROLE:-}"

fail() {
  echo "execution-source-retirement: $*" >&2
  exit 1
}

[[ "$configserver_database" == "configserver" ]] || fail "source database must be configserver"
[[ "$configserver_schema" =~ ^[a-z_][a-z0-9_]*$ ]] || fail "invalid Config Server schema"
[[ "$runtime_role" =~ ^[a-z_][a-z0-9_]*$ ]] || fail "invalid or missing Config Server runtime role"

destination_ready="$(psql -U "$database_user" -d operations -X -tAc \
  "SELECT EXISTS(SELECT 1 FROM operational_meta.operational_schema_migration_t WHERE migration_owner='execution-store' AND schema_name='execution_ops' AND migration_id='0001_execution_foundations')")"
[[ "$destination_ready" == "t" ]] || fail "execution destination is not authoritative"

role_exists="$(psql -U "$database_user" -d "$configserver_database" -X -tAc \
  "SELECT EXISTS(SELECT 1 FROM pg_roles WHERE rolname='${runtime_role}')")"
[[ "$role_exists" == "t" ]] || fail "Config Server runtime role does not exist"

tables=(
  runner_session_t runner_backend_t runner_scheduling_request_t
  execution_session_t execution_session_cleanup_request_t execution_attempt_t
  execution_credential_grant_audit_t execution_fixed_action_t execution_input_t
  execution_provenance_t execution_runtime_audit_t execution_runtime_tool_manifest_t
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

echo "Retired Config Server execution writes for role $runtime_role; source rows were not copied."
