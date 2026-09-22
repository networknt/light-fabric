SET search_path TO workflow_ops, pg_catalog;

CREATE TABLE workflow_ops.task_asst_t (
    host_id uuid NOT NULL,
    task_asst_id uuid NOT NULL,
    task_id uuid NOT NULL,
    assigned_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    assignee_id character varying(255) NOT NULL,
    assignment_type character varying(16) NOT NULL,
    assignment_id character varying(255) NOT NULL,
    assignment_status_code character varying(16) DEFAULT 'ASSIGNED' NOT NULL,
    claimed_by character varying(255),
    claimed_ts timestamp with time zone,
    claim_expires_ts timestamp with time zone,
    reason_code character varying(126),
    category_code character varying(126),
    decision jsonb,
    decision_comment text,
    completion_id uuid,
    completion_idempotency_key character varying(128),
    completed_ts timestamp with time zone,
    aggregate_version bigint DEFAULT 1 NOT NULL,
    active boolean DEFAULT true NOT NULL,
    update_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    update_user character varying(126) DEFAULT SESSION_USER NOT NULL,
    CONSTRAINT task_asst_t_pkey PRIMARY KEY (host_id, task_asst_id),
    CONSTRAINT task_asst_t_task_fkey FOREIGN KEY (host_id, task_id)
        REFERENCES workflow_ops.task_info_t(host_id, task_id) ON DELETE RESTRICT,
    CONSTRAINT task_asst_t_assignment_type_ck CHECK (assignment_type IN ('USER','ROLE')),
    CONSTRAINT task_asst_t_assignment_status_ck CHECK (assignment_status_code IN ('ASSIGNED','CLAIMED','COMPLETED','CANCELLED')),
    CONSTRAINT task_asst_t_version_ck CHECK (aggregate_version > 0),
    CONSTRAINT task_asst_t_claim_ck CHECK (
        (assignment_status_code = 'CLAIMED' AND claimed_by IS NOT NULL AND claimed_ts IS NOT NULL AND claim_expires_ts IS NOT NULL)
        OR assignment_status_code <> 'CLAIMED'
    ),
    CONSTRAINT task_asst_t_completion_ck CHECK (
        (assignment_status_code = 'COMPLETED' AND completion_id IS NOT NULL AND completed_ts IS NOT NULL AND decision IS NOT NULL AND active = FALSE)
        OR assignment_status_code <> 'COMPLETED'
    )
);

CREATE UNIQUE INDEX task_asst_t_live_target_uq
    ON workflow_ops.task_asst_t(host_id, task_id, assignment_type, assignment_id, COALESCE(category_code, ''))
    WHERE active;

CREATE UNIQUE INDEX task_asst_t_completion_idempotency_uq
    ON workflow_ops.task_asst_t(host_id, completion_idempotency_key)
    WHERE completion_idempotency_key IS NOT NULL;

CREATE INDEX task_asst_t_user_inbox_idx
    ON workflow_ops.task_asst_t(host_id, assignment_type, assignment_id, assignment_status_code, assigned_ts DESC)
    WHERE active;

CREATE INDEX task_asst_t_task_idx
    ON workflow_ops.task_asst_t(host_id, task_id);

REVOKE ALL ON TABLE workflow_ops.task_asst_t FROM PUBLIC;
GRANT SELECT, INSERT, UPDATE ON TABLE workflow_ops.task_asst_t TO operations_workflow_runtime;
