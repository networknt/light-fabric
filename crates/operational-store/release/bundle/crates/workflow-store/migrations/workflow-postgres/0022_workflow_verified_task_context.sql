BEGIN;

-- Immutable identity evidence, never credentials. Strict admission is opt-in
-- from trusted accepted definition metadata; legacy invocation rows are intact.
CREATE TABLE workflow_ops.workflow_verified_invocation_t (
    host_id uuid NOT NULL,
    run_id uuid NOT NULL,
    profile varchar(32) NOT NULL CHECK (profile IN ('capture-v1','supplied-input-v2')),
    creator jsonb NOT NULL CHECK (jsonb_typeof(creator)='object'),
    PRIMARY KEY(host_id,run_id),
    FOREIGN KEY(host_id,run_id) REFERENCES workflow_ops.workflow_invocation_t(host_id,workflow_instance_id)
);

CREATE TABLE workflow_ops.workflow_verified_task_context_t (
    host_id uuid NOT NULL,
    action_id uuid NOT NULL,
    run_id uuid NOT NULL,
    task_id uuid NOT NULL,
    context jsonb NOT NULL CHECK (jsonb_typeof(context)='object'),
    PRIMARY KEY(host_id,action_id),
    UNIQUE(host_id,run_id,task_id),
    FOREIGN KEY(host_id,action_id) REFERENCES workflow_ops.workflow_action_permit_t(host_id,action_id),
    FOREIGN KEY(host_id,run_id) REFERENCES workflow_ops.workflow_verified_invocation_t(host_id,run_id),
    FOREIGN KEY(host_id,task_id) REFERENCES workflow_ops.task_info_t(host_id,task_id)
);

-- Deliberately no UPDATE privilege: retries must compare immutable rows.
CREATE TABLE workflow_ops.workflow_verified_task_result_t (
    host_id uuid NOT NULL,
    action_id uuid NOT NULL,
    result jsonb NOT NULL CHECK (jsonb_typeof(result)='object'),
    PRIMARY KEY(host_id,action_id),
    FOREIGN KEY(host_id,action_id) REFERENCES workflow_ops.workflow_verified_task_context_t(host_id,action_id)
);

GRANT SELECT,INSERT,DELETE ON workflow_ops.workflow_verified_invocation_t,
    workflow_ops.workflow_verified_task_context_t,workflow_ops.workflow_verified_task_result_t TO operations_workflow_runtime;

COMMIT;
