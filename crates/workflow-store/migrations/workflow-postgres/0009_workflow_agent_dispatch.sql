-- Workflow owns dispatch intent and the authenticated result mirror. Agent owns
-- its job/session/turn records; this table is NOT a shared Agent runtime table.
CREATE TABLE workflow_ops.workflow_agent_job_t (
 host_id uuid NOT NULL, job_id uuid NOT NULL,
 workflow_process_id uuid NOT NULL, workflow_task_id uuid NOT NULL,
 agent_def_id uuid NOT NULL, idempotency_key text NOT NULL,
 input jsonb NOT NULL, input_schema_digest text NOT NULL, output_schema jsonb NOT NULL,
 deadline_ts timestamptz NOT NULL, token_budget bigint NOT NULL CHECK(token_budget>0),
 cost_budget_micros bigint NOT NULL CHECK(cost_budget_micros>=0),
 delegation_depth integer NOT NULL CHECK(delegation_depth>=0),
 maximum_delegation_depth integer NOT NULL CHECK(maximum_delegation_depth>=delegation_depth),
 state text NOT NULL DEFAULT 'PENDING' CHECK(state IN('PENDING','TURN_CREATED','RUNNING','SUCCEEDED','FAILED','CANCELLED','UNKNOWN')),
 public_output jsonb, error jsonb, report jsonb,
 cancellation_requested_ts timestamptz,
 created_ts timestamptz NOT NULL DEFAULT now(), updated_ts timestamptz NOT NULL DEFAULT now(),
 PRIMARY KEY(host_id,job_id), UNIQUE(host_id,idempotency_key), UNIQUE(host_id,workflow_task_id),
 FOREIGN KEY(host_id,workflow_task_id) REFERENCES workflow_ops.task_info_t(host_id,task_id),
 FOREIGN KEY(host_id,workflow_process_id) REFERENCES workflow_ops.process_info_t(host_id,process_id)
);
GRANT SELECT,INSERT,UPDATE,DELETE ON workflow_ops.workflow_agent_job_t TO operations_workflow_runtime;
