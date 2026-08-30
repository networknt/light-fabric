#!/usr/bin/env bash
set -euo pipefail

[[ "${OPERATIONAL_RESET_CONFIRM:-}" == "RESET_WORKFLOW_OPS" ]] || {
  echo "reset-workflow-store: set OPERATIONAL_RESET_CONFIRM=RESET_WORKFLOW_OPS" >&2
  exit 2
}

database_user="${POSTGRES_USER:-postgres}"
database_name="${PORTAL_DB_OPERATIONAL_NAME:-operations}"
[[ "$database_name" == "operations" ]] || {
  echo "reset-workflow-store: database identity must be operations" >&2
  exit 2
}

ready="$(psql -U "$database_user" -d "$database_name" -X -tAc \
  "SELECT EXISTS(SELECT 1 FROM operational_meta.operational_store_binding_t WHERE active) AND EXISTS(SELECT 1 FROM operational_meta.operational_schema_migration_t WHERE migration_owner='workflow-store' AND schema_name='workflow_ops' AND migration_id='0001_workflow_runtime')")"
[[ "$ready" == "t" ]] || {
  echo "reset-workflow-store: operational scope or Workflow schema is missing" >&2
  exit 1
}

psql -U "$database_user" -d "$database_name" -X --set=ON_ERROR_STOP=1 <<'SQL' >/dev/null
TRUNCATE TABLE
  workflow_ops.workflow_tool_approval_evidence_t,
  workflow_ops.workflow_tool_access_request_item_t,
  workflow_ops.workflow_tool_access_request_t,
  workflow_ops.workflow_task_effect_t,
  workflow_ops.workflow_invocation_idempotency_t,
  workflow_ops.workflow_invocation_budget_reservation_t,
  workflow_ops.workflow_invocation_budget_t,
  workflow_ops.workflow_invocation_event_quarantine_t,
  workflow_ops.workflow_invocation_audit_outbox_t,
  workflow_ops.workflow_invocation_t,
  workflow_ops.workflow_fork_branch_t,
  workflow_ops.workflow_fork_join_t,
  workflow_ops.workflow_executor_tenant_turn_t,
  workflow_ops.workflow_artifact_t,
  workflow_ops.workflow_approval_t,
  workflow_ops.task_info_t,
  workflow_ops.process_info_t,
  workflow_ops.workflow_tool_dependency_t,
  workflow_ops.workflow_tool_grant_t,
  workflow_ops.workflow_tool_binding_t,
  workflow_ops.workflow_a2a_binding_t,
  workflow_ops.workflow_endpoint_target_t,
  workflow_ops.workflow_execution_policy_t,
  workflow_ops.wf_definition_t
RESTART IDENTITY CASCADE;
SQL

remaining="$(psql -U "$database_user" -d "$database_name" -X -tAc \
  "SELECT COALESCE(sum(row_count),0)::bigint FROM (
     SELECT (xpath('/row/count/text()',query_to_xml(
       format('SELECT count(*) AS count FROM workflow_ops.%I',tablename),false,true,''
     )))[1]::text::bigint AS row_count
     FROM pg_tables WHERE schemaname='workflow_ops'
   ) counts")"
[[ "${remaining:-0}" == "0" ]] || {
  echo "reset-workflow-store: Workflow rows remain after reset" >&2
  exit 1
}
echo "Reset operations.workflow_ops; Agent, A2A, execution, Config Server, and Knowledge data were not touched."
