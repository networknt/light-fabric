-- A Portal Tool-access request starts at most one operational approval run.
-- This is Workflow-owned state; the Portal request and final grant stay event-backed.
CREATE TABLE IF NOT EXISTS workflow_ops.workflow_tool_access_approval_run_t (
    host_id uuid NOT NULL,
    request_id uuid NOT NULL,
    request_digest varchar(71) NOT NULL,
    request_version bigint NOT NULL,
    approval_wf_def_id uuid NOT NULL,
    approval_definition_digest varchar(71) NOT NULL,
    requester_user_id uuid NOT NULL,
    workflow_instance_id uuid NOT NULL,
    process_id uuid NOT NULL,
    decision_id uuid,
    decision_idempotency_key varchar(128),
    decision_kind varchar(8),
    decision_payload_digest varchar(71),
    approver_user_id uuid,
    approver_claims_digest varchar(71),
    task_id uuid,
    task_asst_id uuid,
    decision_comment text,
    delivery_state varchar(16),
    portal_outcome varchar(16),
    portal_committed_ts timestamptz,
    decision_ts timestamptz,
    acknowledged_ts timestamptz,
    attempt_count bigint NOT NULL DEFAULT 0,
    last_attempt_ts timestamptz,
    last_delivery_error varchar(32),
    accepted_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (host_id, request_id),
    UNIQUE (host_id, workflow_instance_id),
    UNIQUE (host_id, decision_id),
    FOREIGN KEY (host_id, workflow_instance_id)
        REFERENCES workflow_ops.workflow_invocation_t(host_id, workflow_instance_id)
        ON DELETE RESTRICT,
    CHECK (request_digest ~ '^sha256:[0-9a-f]{64}$'),
    CHECK (approval_definition_digest ~ '^sha256:[0-9a-f]{64}$'),
    CHECK (request_version > 0),
    CHECK (decision_kind IS NULL OR decision_kind IN ('APPROVE','REJECT')),
    CHECK (delivery_state IS NULL OR delivery_state IN ('PENDING','ACKED','BLOCKED')),
    CHECK ((decision_id IS NULL) = (delivery_state IS NULL)),
    CHECK (decision_payload_digest IS NULL OR decision_payload_digest ~ '^sha256:[0-9a-f]{64}$'),
    CHECK (approver_claims_digest IS NULL OR approver_claims_digest ~ '^sha256:[0-9a-f]{64}$')
);

CREATE INDEX IF NOT EXISTS workflow_tool_access_approval_pending_idx
    ON workflow_ops.workflow_tool_access_approval_run_t (decision_ts)
    WHERE delivery_state IN ('PENDING','BLOCKED');

ALTER TABLE workflow_ops.task_asst_t
    DROP CONSTRAINT IF EXISTS task_asst_t_assignment_status_ck;
ALTER TABLE workflow_ops.task_asst_t
    ADD CONSTRAINT task_asst_t_assignment_status_ck
    CHECK (assignment_status_code IN ('ASSIGNED','CLAIMED','DECISION_PENDING','COMPLETED','CANCELLED'));
