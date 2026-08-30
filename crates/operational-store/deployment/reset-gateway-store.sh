#!/usr/bin/env bash
set -euo pipefail

[[ "${OPERATIONAL_RESET_CONFIRM:-}" == "RESET_GATEWAY_OPS" ]] || {
  echo "reset-gateway-store: set OPERATIONAL_RESET_CONFIRM=RESET_GATEWAY_OPS" >&2
  exit 2
}

database_name="${PORTAL_DB_OPERATIONAL_NAME:-operations}"
database_user="${POSTGRES_USER:-postgres}"

[[ "$database_name" == "operations" ]] || {
  echo "reset-gateway-store: database identity must be operations" >&2
  exit 1
}

ready="$(psql -U "$database_user" -d "$database_name" -X -tAc \
  "SELECT EXISTS(SELECT 1 FROM operational_meta.operational_store_binding_t WHERE active) AND EXISTS(SELECT 1 FROM operational_meta.operational_schema_migration_t WHERE migration_owner='gateway-operational-store' AND schema_name='gateway_ops' AND migration_id='0001_gateway_evidence_spool')")"
[[ "$ready" == "t" ]] || {
  echo "reset-gateway-store: operational scope or Gateway schema is missing" >&2
  exit 1
}

psql -U "$database_user" -d "$database_name" -X --set=ON_ERROR_STOP=1 <<'SQL'
TRUNCATE TABLE
  gateway_ops.gateway_evidence_spool_t,
  gateway_ops.gateway_evidence_quota_t;
SQL

remaining="$(psql -U "$database_user" -d "$database_name" -X -tAc \
  "SELECT count(*) FROM gateway_ops.gateway_evidence_spool_t")"
[[ "$remaining" == "0" ]] || { echo "reset-gateway-store: Gateway rows remain" >&2; exit 1; }

echo "Reset disposable Gateway operational evidence in operations.gateway_ops."
