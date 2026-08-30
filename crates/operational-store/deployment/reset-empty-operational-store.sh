#!/usr/bin/env bash
set -euo pipefail

[[ "${OPERATIONAL_RESET_CONFIRM:-}" == "DELETE_EMPTY_OPERATIONS" ]] || {
  echo "reset-empty-operational-store: set OPERATIONAL_RESET_CONFIRM=DELETE_EMPTY_OPERATIONS" >&2
  exit 2
}

database_user="${POSTGRES_USER:-postgres}"
application_table_count="$(psql -U "$database_user" -d operations -X -tAc \
  "SELECT count(*) FROM pg_tables WHERE schemaname IN ('execution_ops','agent_ops','a2a_ops','workflow_ops','gateway_ops','audit_ops','artifact_ops')")"
[[ "$application_table_count" == "0" ]] || {
  echo "reset-empty-operational-store: operational application tables exist; Phase 1 reset refused" >&2
  exit 1
}

preserved_databases="$(psql -U "$database_user" -d postgres -X -tAc \
  "SELECT count(*) FROM pg_database WHERE datname IN ('configserver','knowledge')")"
[[ "$preserved_databases" == "2" ]] || {
  echo "reset-empty-operational-store: configserver and knowledge must both exist" >&2
  exit 1
}

psql -U "$database_user" -d postgres -X --set=ON_ERROR_STOP=1 \
  -c "DROP DATABASE operations WITH (FORCE)" >/dev/null
echo "Removed the empty Phase 1 operations database; configserver and knowledge were preserved."

