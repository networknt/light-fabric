#!/usr/bin/env bash
set -euo pipefail

[[ "${OPERATIONAL_RESET_CONFIRM:-}" == "RESET_AGENT_OPS" ]] || {
  echo "reset-agent-store: set OPERATIONAL_RESET_CONFIRM=RESET_AGENT_OPS" >&2
  exit 2
}

database_user="${POSTGRES_USER:-postgres}"
database_name="${PORTAL_DB_OPERATIONAL_NAME:-operations}"
[[ "$database_name" == "operations" ]] || {
  echo "reset-agent-store: database identity must be operations" >&2
  exit 2
}

scope_ready="$(psql -U "$database_user" -d "$database_name" -X -tAc \
  "SELECT current_database()='operations' AND EXISTS(SELECT 1 FROM operational_meta.operational_store_binding_t WHERE active) AND EXISTS(SELECT 1 FROM operational_meta.operational_schema_migration_t WHERE migration_owner='agent-store' AND schema_name='agent_ops' AND migration_id='0001_agent_and_embedded_memory')")"
[[ "$scope_ready" == "t" ]] || {
  echo "reset-agent-store: operational scope or Agent schema is missing" >&2
  exit 1
}

psql -U "$database_user" -d "$database_name" -X --set=ON_ERROR_STOP=1 <<'SQL' >/dev/null
TRUNCATE TABLE
    agent_ops.agent_a2a_artifact_t,
    agent_ops.agent_a2a_task_alias_t,
    agent_ops.agent_a2a_context_alias_t,
    agent_ops.agent_execution_outbox_t,
    agent_ops.operational_reference_reconciliation_t,
    agent_ops.operational_reference_evidence_t,
    agent_ops.runtime_operational_scope_t,
    agent_ops.agent_session_event_t,
    agent_ops.agent_session_history_t,
    agent_ops.agent_quota_reservation_t,
    agent_ops.agent_quota_usage_t,
    agent_ops.agent_turn_materialization_t,
    agent_ops.agent_action_attempt_t,
    agent_ops.agent_approval_t,
    agent_ops.agent_fixed_action_t,
    agent_ops.agent_job_t,
    agent_ops.agent_turn_t,
    agent_ops.agent_session_t,
    agent_ops.agent_policy_snapshot_t,
    agent_ops.agent_delegation_replay_t,
    agent_ops.agent_memory_unit_entity_t,
    agent_ops.agent_memory_entity_cooccur_t,
    agent_ops.agent_memory_link_t,
    agent_ops.agent_memory_reflection_t,
    agent_ops.agent_memory_unit_t,
    agent_ops.agent_memory_entity_t,
    agent_ops.agent_memory_doc_t,
    agent_ops.agent_memory_bank_t
RESTART IDENTITY CASCADE;
SQL

remaining="$(psql -U "$database_user" -d "$database_name" -X -tAc \
  "SELECT COALESCE(sum(row_count),0)::bigint FROM (
     SELECT (xpath('/row/count/text()',query_to_xml(
       format('SELECT count(*) AS count FROM agent_ops.%I',tablename),false,true,''
     )))[1]::text::bigint AS row_count
     FROM pg_tables WHERE schemaname='agent_ops'
   ) counts")"
[[ "${remaining:-0}" == "0" ]] || {
  echo "reset-agent-store: Agent rows remain after reset" >&2
  exit 1
}
echo "Reset operations.agent_ops including native A2A aliases; external A2A, Workflow, Config Server, execution, and Knowledge data were not touched."
