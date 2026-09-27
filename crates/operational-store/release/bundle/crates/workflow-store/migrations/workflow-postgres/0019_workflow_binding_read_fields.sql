SET search_path TO workflow_ops, pg_catalog;

-- Preserve the exact optional Tool schemas hashed in each immutable binding revision,
-- and its carry-over denial independently of expiring operation receipts.
ALTER TABLE workflow_ops.workflow_tool_binding_t
    ADD COLUMN IF NOT EXISTS input_schema jsonb,
    ADD COLUMN IF NOT EXISTS output_schema jsonb,
    ADD COLUMN IF NOT EXISTS carry_over_denied_reason text;

ALTER TABLE workflow_ops.workflow_tool_binding_t
    ADD CONSTRAINT workflow_tool_binding_input_schema_object_ck
        CHECK (input_schema IS NULL OR jsonb_typeof(input_schema) = 'object'),
    ADD CONSTRAINT workflow_tool_binding_output_schema_object_ck
        CHECK (output_schema IS NULL OR jsonb_typeof(output_schema) = 'object');
