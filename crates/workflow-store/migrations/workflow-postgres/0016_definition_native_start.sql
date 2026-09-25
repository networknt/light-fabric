SET search_path TO workflow_ops, pg_catalog;

-- Native definition starts have no workflow-backed Tool binding. Existing
-- workflow-backed invocation rows retain their binding and foreign key.
ALTER TABLE workflow_ops.workflow_invocation_t
    ALTER COLUMN binding_id DROP NOT NULL;
