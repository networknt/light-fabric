-- Runtime startup validators may confirm their own immutable migration row.
-- They receive no write authority on the ledger.

GRANT SELECT ON operational_meta.operational_schema_migration_t TO
    operations_execution_runtime,
    operations_agent_runtime,
    operations_a2a_runtime,
    operations_workflow_runtime,
    operations_gateway_runtime,
    operations_audit_publisher,
    operations_artifact_runtime;
