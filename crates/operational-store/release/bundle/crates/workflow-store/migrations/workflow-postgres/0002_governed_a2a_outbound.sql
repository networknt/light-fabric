-- Accepted outbound A2A bindings are local immutable projections. Workflow
-- authors select agent_ref only; model/task input cannot supply a destination.

CREATE TABLE workflow_ops.workflow_a2a_binding_t (
  host_id UUID NOT NULL,
  binding_id UUID NOT NULL,
  agent_ref VARCHAR(256) NOT NULL,
  publication_id UUID NOT NULL,
  policy_digest VARCHAR(80) NOT NULL,
  gateway_uri TEXT NOT NULL,
  audience VARCHAR(64) NOT NULL DEFAULT 'light-a2a',
  projection_digest VARCHAR(80) NOT NULL,
  active BOOLEAN NOT NULL DEFAULT TRUE,
  updated_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY(host_id, binding_id),
  UNIQUE(host_id, agent_ref),
  CHECK (agent_ref ~ '^[A-Za-z0-9][A-Za-z0-9._:-]{0,255}$'),
  CHECK (policy_digest ~ '^sha256:[0-9a-f]{64}$'),
  CHECK (projection_digest ~ '^sha256:[0-9a-f]{64}$'),
  CHECK (gateway_uri ~ '^https?://'),
  CHECK (audience IN ('light-a2a','light-agent'))
);

GRANT SELECT, INSERT, UPDATE, DELETE ON workflow_ops.workflow_a2a_binding_t
  TO operations_workflow_runtime;
