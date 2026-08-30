#!/usr/bin/env bash
set -euo pipefail

[[ "${OPERATIONAL_RESET_CONFIRM:-}" == "RESET_EXECUTION_OPS" ]] || {
  echo "reset-execution-store: set OPERATIONAL_RESET_CONFIRM=RESET_EXECUTION_OPS" >&2
  exit 2
}

database_user="${POSTGRES_USER:-postgres}"
database_name="${PORTAL_DB_OPERATIONAL_NAME:-operations}"
[[ "$database_name" == "operations" ]] || {
  echo "reset-execution-store: database identity must be operations" >&2
  exit 2
}

scope_ready="$(psql -U "$database_user" -d "$database_name" -X -tAc \
  "SELECT current_database()='operations' AND EXISTS(SELECT 1 FROM operational_meta.operational_store_binding_t WHERE active) AND to_regclass('execution_ops.runner_scheduling_request_t') IS NOT NULL")"
[[ "$scope_ready" == "t" ]] || {
  echo "reset-execution-store: operational scope or execution schema is missing" >&2
  exit 1
}

psql -U "$database_user" -d "$database_name" -X --set=ON_ERROR_STOP=1 <<'SQL' >/dev/null
TRUNCATE TABLE
    execution_ops.execution_runtime_tool_manifest_t,
    execution_ops.execution_runtime_audit_t,
    execution_ops.execution_provenance_t,
    execution_ops.execution_input_t,
    execution_ops.execution_fixed_action_t,
    execution_ops.execution_credential_grant_audit_t,
    execution_ops.execution_session_cleanup_request_t,
    execution_ops.execution_attempt_t,
    execution_ops.execution_session_t,
    execution_ops.runner_scheduling_request_t,
    execution_ops.runner_backend_t,
    execution_ops.runner_session_t
RESTART IDENTITY CASCADE;
SQL

remaining="$(psql -U "$database_user" -d "$database_name" -X -tAc \
  "SELECT sum(n_live_tup)::bigint FROM pg_stat_user_tables WHERE schemaname='execution_ops'")"
[[ "${remaining:-0}" == "0" ]] || {
  echo "reset-execution-store: execution rows remain after reset" >&2
  exit 1
}
echo "Reset operations.execution_ops; Config Server and Knowledge data were not touched."
