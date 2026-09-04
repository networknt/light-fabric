BEGIN;

ALTER TABLE workflow_ops.workflow_endpoint_target_t
    ALTER COLUMN endpoint_ref TYPE character varying(512);

ALTER TABLE workflow_ops.workflow_endpoint_target_t
    ADD COLUMN resolution_document jsonb;

ALTER TABLE workflow_ops.workflow_endpoint_target_t
    ADD CONSTRAINT workflow_endpoint_target_t_resolution_document_check
    CHECK (resolution_document IS NULL OR jsonb_typeof(resolution_document) = 'object');

COMMIT;
