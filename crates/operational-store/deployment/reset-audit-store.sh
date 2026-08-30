#!/usr/bin/env bash
set -euo pipefail

[[ "${OPERATIONAL_RESET_CONFIRM:-}" == "RESET_AUDIT_OPS" ]] || {
  echo "reset-audit-store: set OPERATIONAL_RESET_CONFIRM=RESET_AUDIT_OPS" >&2
  exit 2
}

database_name="${PORTAL_DB_OPERATIONAL_NAME:-operations}"
database_user="${POSTGRES_USER:-postgres}"

[[ "$database_name" == "operations" ]] || {
  echo "reset-audit-store: database identity must be operations" >&2
  exit 1
}

ready="$(psql -U "$database_user" -d "$database_name" -X -tAc \
  "SELECT EXISTS(SELECT 1 FROM operational_meta.operational_store_binding_t WHERE active) AND EXISTS(SELECT 1 FROM operational_meta.operational_schema_migration_t WHERE migration_owner='audit-store' AND schema_name='audit_ops' AND migration_id='0001_tenant_audit_store')")"
[[ "$ready" == "t" ]] || {
  echo "reset-audit-store: operational scope or audit schema is missing" >&2
  exit 1
}

psql -U "$database_user" -d "$database_name" -X --set=ON_ERROR_STOP=1 <<'SQL'
TRUNCATE TABLE
  audit_ops.audit_delivery_t,
  audit_ops.audit_hold_t,
  audit_ops.audit_record_t;
SQL

remaining="$(psql -U "$database_user" -d "$database_name" -X -tAc \
  "SELECT count(*) FROM audit_ops.audit_record_t")"
[[ "$remaining" == "0" ]] || { echo "reset-audit-store: audit rows remain" >&2; exit 1; }

echo "Reset disposable tenant audit evidence in operations.audit_ops."
