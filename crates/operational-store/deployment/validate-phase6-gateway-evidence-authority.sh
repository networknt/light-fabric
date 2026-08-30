#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
database_name="${PORTAL_DB_OPERATIONAL_NAME:-operations}"
database_user="${POSTGRES_USER:-postgres}"

bash "$script_dir/validate-phase5-workflow-a2a-authority.sh"

ready="$(psql -U "$database_user" -d "$database_name" -X -tA <<'SQL'
SELECT to_regclass('gateway_ops.gateway_evidence_quota_t') IS NOT NULL
   AND to_regclass('gateway_ops.gateway_evidence_spool_t') IS NOT NULL
   AND to_regclass('audit_ops.audit_record_t') IS NOT NULL
   AND to_regclass('audit_ops.audit_delivery_t') IS NOT NULL
   AND to_regclass('audit_ops.audit_hold_t') IS NOT NULL
   AND to_regclass('artifact_ops.artifact_metadata_t') IS NOT NULL
   AND to_regclass('artifact_ops.artifact_relationship_t') IS NOT NULL
   AND to_regclass('artifact_ops.artifact_hold_t') IS NOT NULL
   AND to_regclass('artifact_ops.artifact_event_t') IS NOT NULL
   AND has_table_privilege('operations_gateway_runtime','gateway_ops.gateway_evidence_spool_t','SELECT,INSERT,UPDATE,DELETE')
   AND NOT has_schema_privilege('operations_gateway_runtime','gateway_ops','CREATE')
   AND NOT has_schema_privilege('operations_gateway_runtime','audit_ops','USAGE')
   AND has_table_privilege('operations_audit_publisher','audit_ops.audit_record_t','SELECT,INSERT,UPDATE')
   AND NOT has_table_privilege('operations_audit_publisher','audit_ops.audit_record_t','DELETE')
   AND NOT has_schema_privilege('operations_audit_publisher','gateway_ops','USAGE')
   AND has_table_privilege('operations_artifact_runtime','artifact_ops.artifact_metadata_t','SELECT,INSERT,UPDATE')
   AND NOT has_table_privilege('operations_artifact_runtime','artifact_ops.artifact_metadata_t','DELETE')
   AND NOT has_schema_privilege('operations_artifact_runtime','agent_ops','USAGE');
SQL
)"
[[ "$ready" == "t" ]] || {
  echo "phase6-authority-validation: Gateway, audit, or artifact boundary is invalid" >&2
  exit 1
}

gateway_application_state="$(psql -U "$database_user" -d "$database_name" -X -tA <<'SQL'
SELECT count(*) = 0
FROM information_schema.columns
WHERE table_schema='gateway_ops'
  AND column_name ~ '(task|session|turn|process|artifact)_id';
SQL
)"
[[ "$gateway_application_state" == "t" ]] || {
  echo "phase6-authority-validation: gateway_ops contains forbidden application state" >&2
  exit 1
}

echo "Phase 6 Gateway, audit, artifact, and traffic-evidence operational boundaries are ready."
