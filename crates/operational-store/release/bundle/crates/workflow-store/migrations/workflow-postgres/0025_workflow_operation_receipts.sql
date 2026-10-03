-- V2 exact immutable acceptance evidence. No historical receipt backfill.
BEGIN;
CREATE TABLE workflow_ops.workflow_operation_receipt_t (
    host_id uuid NOT NULL,
    principal_subject varchar(255) NOT NULL,
    end_user_subject varchar(255) NOT NULL,
    operation_kind varchar(64) NOT NULL,
    caller_key text NOT NULL CHECK (octet_length(caller_key) BETWEEN 1 AND 512),
    request_text text NOT NULL,
    request_digest varchar(71) NOT NULL CHECK (request_digest ~ '^sha256:[0-9a-f]{64}$'),
    selected_content jsonb NOT NULL CHECK (jsonb_typeof(selected_content)='object'),
    receipt jsonb NOT NULL CHECK (jsonb_typeof(receipt)='object'),
    invocation_status jsonb CHECK (invocation_status IS NULL OR jsonb_typeof(invocation_status)='object'),
    accepted_ts timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY(host_id,principal_subject,end_user_subject,operation_kind,caller_key)
);
COMMENT ON TABLE workflow_ops.workflow_operation_receipt_t IS
    'Immutable original v2 acceptance; indefinite retention pending separately reviewed archival policy. Rollback retains all rows. No FK to mutable or reconstructible state.';
CREATE FUNCTION workflow_ops.workflow_receipt_immutable() RETURNS trigger
LANGUAGE plpgsql AS $$ BEGIN
    RAISE EXCEPTION 'WORKFLOW_RECEIPT_IMMUTABLE';
END $$;
CREATE TRIGGER workflow_receipt_no_mutation BEFORE UPDATE OR DELETE
ON workflow_ops.workflow_operation_receipt_t FOR EACH ROW
EXECUTE FUNCTION workflow_ops.workflow_receipt_immutable();
CREATE TRIGGER workflow_receipt_no_truncate BEFORE TRUNCATE
ON workflow_ops.workflow_operation_receipt_t FOR EACH STATEMENT
EXECUTE FUNCTION workflow_ops.workflow_receipt_immutable();
-- Explicitly remove direct grants inherited from the migrator's default ACLs.
REVOKE ALL ON workflow_ops.workflow_operation_receipt_t FROM PUBLIC, operations_workflow_runtime;
GRANT SELECT, INSERT ON workflow_ops.workflow_operation_receipt_t TO operations_workflow_runtime;
REVOKE ALL ON FUNCTION workflow_ops.workflow_receipt_immutable() FROM PUBLIC;
COMMIT;
