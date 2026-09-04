BEGIN;

ALTER TABLE workflow_ops.workflow_tool_binding_t
    ADD COLUMN tool_name character varying(126);

ALTER TABLE workflow_ops.workflow_tool_binding_t
    ADD CONSTRAINT workflow_tool_binding_t_tool_name_check
    CHECK (tool_name IS NULL OR btrim(tool_name) <> '');

ALTER TABLE workflow_ops.workflow_endpoint_target_t
    DROP CONSTRAINT workflow_endpoint_target_t_pkey;

ALTER TABLE workflow_ops.workflow_endpoint_target_t
    ADD CONSTRAINT workflow_endpoint_target_t_pkey
    PRIMARY KEY (host_id, binding_id, endpoint_ref);

COMMIT;
