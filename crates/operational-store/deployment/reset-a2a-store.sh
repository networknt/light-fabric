#!/usr/bin/env bash
set -euo pipefail

[[ "${OPERATIONAL_RESET_CONFIRM:-}" == "RESET_A2A_OPS" ]] || {
  echo "reset-a2a-store: set OPERATIONAL_RESET_CONFIRM=RESET_A2A_OPS" >&2
  exit 2
}

database_user="${POSTGRES_USER:-postgres}"
database_name="${PORTAL_DB_OPERATIONAL_NAME:-operations}"
[[ "$database_name" == "operations" ]] || {
  echo "reset-a2a-store: database identity must be operations" >&2
  exit 2
}

ready="$(psql -U "$database_user" -d "$database_name" -X -tAc \
  "SELECT EXISTS(SELECT 1 FROM operational_meta.operational_store_binding_t WHERE active) AND EXISTS(SELECT 1 FROM operational_meta.operational_schema_migration_t WHERE migration_owner='a2a-store' AND schema_name='a2a_ops' AND migration_id='0001_external_a2a_durability')")"
[[ "$ready" == "t" ]] || {
  echo "reset-a2a-store: operational scope or A2A schema is missing" >&2
  exit 1
}

psql -U "$database_user" -d "$database_name" -X --set=ON_ERROR_STOP=1 <<'SQL' >/dev/null
TRUNCATE TABLE
  a2a_ops.a2a_artifact_t,
  a2a_ops.a2a_callback_t,
  a2a_ops.a2a_backend_correlation_t,
  a2a_ops.a2a_delegation_replay_t,
  a2a_ops.a2a_audit_outbox_t,
  a2a_ops.a2a_task_event_t,
  a2a_ops.a2a_message_idempotency_t,
  a2a_ops.a2a_task_t,
  a2a_ops.a2a_context_t
RESTART IDENTITY CASCADE;
SQL

remaining="$(psql -U "$database_user" -d "$database_name" -X -tAc \
  "SELECT COALESCE(sum(row_count),0)::bigint FROM (
     SELECT (xpath('/row/count/text()',query_to_xml(
       format('SELECT count(*) AS count FROM a2a_ops.%I',tablename),false,true,''
     )))[1]::text::bigint AS row_count
     FROM pg_tables WHERE schemaname='a2a_ops'
   ) counts")"
[[ "${remaining:-0}" == "0" ]] || {
  echo "reset-a2a-store: A2A rows remain after reset" >&2
  exit 1
}
echo "Reset operations.a2a_ops; native Agent A2A, Workflow, execution, Config Server, and Knowledge data were not touched."
