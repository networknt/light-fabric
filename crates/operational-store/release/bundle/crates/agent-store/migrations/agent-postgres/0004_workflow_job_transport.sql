-- Agent-local acknowledgement of the authenticated Workflow result mirror.
ALTER TABLE agent_ops.agent_job_t ADD COLUMN workflow_reported_ts timestamptz;
