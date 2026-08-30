#!/usr/bin/env bash
set -euo pipefail

[[ "${OPERATIONAL_RESET_CONFIRM:-}" == "RESET_ARTIFACT_OPS" ]] || {
  echo "reset-artifact-store: set OPERATIONAL_RESET_CONFIRM=RESET_ARTIFACT_OPS" >&2
  exit 2
}

database_name="${PORTAL_DB_OPERATIONAL_NAME:-operations}"
database_user="${POSTGRES_USER:-postgres}"

[[ "$database_name" == "operations" ]] || {
  echo "reset-artifact-store: database identity must be operations" >&2
  exit 1
}

ready="$(psql -U "$database_user" -d "$database_name" -X -tAc \
  "SELECT EXISTS(SELECT 1 FROM operational_meta.operational_store_binding_t WHERE active) AND EXISTS(SELECT 1 FROM operational_meta.operational_schema_migration_t WHERE migration_owner='artifact-store' AND schema_name='artifact_ops' AND migration_id='0001_artifact_metadata')")"
[[ "$ready" == "t" ]] || {
  echo "reset-artifact-store: operational scope or artifact schema is missing" >&2
  exit 1
}

psql -U "$database_user" -d "$database_name" -X --set=ON_ERROR_STOP=1 <<'SQL'
TRUNCATE TABLE
  artifact_ops.artifact_event_t,
  artifact_ops.artifact_hold_t,
  artifact_ops.artifact_relationship_t,
  artifact_ops.artifact_metadata_t;
SQL

remaining="$(psql -U "$database_user" -d "$database_name" -X -tAc \
  "SELECT count(*) FROM artifact_ops.artifact_metadata_t")"
[[ "$remaining" == "0" ]] || { echo "reset-artifact-store: artifact rows remain" >&2; exit 1; }

echo "Reset disposable artifact metadata in operations.artifact_ops; object bytes are not touched."
