-- Native process deletion and append-only notes. Historical process-only rows
-- are deliberately not enrolled in either operation.
CREATE TABLE workflow_ops.workflow_process_deletion_t (
    host_id uuid NOT NULL,
    process_id uuid NOT NULL,
    workflow_instance_id uuid NOT NULL,
    operation_id uuid NOT NULL,
    idempotency_key varchar(128) NOT NULL,
    requested_lifecycle_version bigint NOT NULL CHECK (requested_lifecycle_version > 0),
    lifecycle_version bigint NOT NULL CHECK (lifecycle_version > requested_lifecycle_version),
    reason varchar(1000) NOT NULL,
    actor_subject varchar(255) NOT NULL,
    requested_ts timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (host_id, process_id),
    UNIQUE (host_id, operation_id),
    FOREIGN KEY (host_id, workflow_instance_id)
        REFERENCES workflow_ops.workflow_invocation_t(host_id, workflow_instance_id)
);
CREATE TABLE workflow_ops.workflow_process_note_t (
    host_id uuid NOT NULL,
    note_id uuid NOT NULL,
    process_id uuid NOT NULL,
    workflow_instance_id uuid NOT NULL,
    task_id uuid,
    idempotency_key varchar(128) NOT NULL,
    note_text varchar(4000) NOT NULL,
    actor_subject varchar(255) NOT NULL,
    created_ts timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (host_id, note_id),
    UNIQUE (host_id, process_id, idempotency_key),
    FOREIGN KEY (host_id, workflow_instance_id)
        REFERENCES workflow_ops.workflow_invocation_t(host_id, workflow_instance_id),
    FOREIGN KEY (host_id, process_id)
        REFERENCES workflow_ops.process_info_t(host_id, process_id)
);
CREATE INDEX workflow_process_note_order_idx
    ON workflow_ops.workflow_process_note_t(host_id, process_id, created_ts, note_id);
GRANT SELECT, INSERT, UPDATE ON workflow_ops.workflow_process_deletion_t TO operations_workflow_runtime;
GRANT SELECT, INSERT ON workflow_ops.workflow_process_note_t TO operations_workflow_runtime;
