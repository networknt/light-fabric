-- Forward repair: 0007 created these Workflow-owned tables without runtime DML.
-- No schema ownership, CREATE, cross-service privileges or token access.
GRANT SELECT, INSERT, UPDATE ON
    workflow_ops.workflow_action_authority_t,
    workflow_ops.workflow_action_permit_t,
    workflow_ops.workflow_action_dispatch_t,
    workflow_ops.workflow_action_audit_t,
    workflow_ops.workflow_gateway_owner_t,
    workflow_ops.workflow_gateway_boot_t
TO operations_workflow_runtime;
