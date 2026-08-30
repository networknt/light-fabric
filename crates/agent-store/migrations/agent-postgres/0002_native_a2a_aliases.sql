-- Native A2A aliases remain inside the Agent transaction boundary. They map
-- protocol IDs onto authoritative Agent sessions/turns and do not create a
-- light-a2a sidecar dependency for LIGHT_AGENT publications.

CREATE TABLE agent_ops.agent_a2a_context_alias_t (
  host_id UUID NOT NULL,
  public_context_id VARCHAR(256) NOT NULL,
  session_id UUID NOT NULL,
  principal_subject VARCHAR(512) NOT NULL,
  agent_def_id UUID NOT NULL,
  publication_id UUID NOT NULL,
  policy_digest VARCHAR(80) NOT NULL,
  expires_ts TIMESTAMPTZ NOT NULL,
  created_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY(host_id, public_context_id),
  FOREIGN KEY(host_id, session_id) REFERENCES agent_ops.agent_session_t(host_id, session_id) ON DELETE CASCADE,
  CHECK (policy_digest ~ '^sha256:[0-9a-f]{64}$')
);

CREATE TABLE agent_ops.agent_a2a_task_alias_t (
  host_id UUID NOT NULL,
  public_task_id VARCHAR(256) NOT NULL,
  public_context_id VARCHAR(256) NOT NULL,
  turn_id UUID NOT NULL,
  principal_subject VARCHAR(512) NOT NULL,
  agent_def_id UUID NOT NULL,
  publication_id UUID NOT NULL,
  policy_digest VARCHAR(80) NOT NULL,
  state VARCHAR(32) NOT NULL DEFAULT 'SUBMITTED',
  created_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY(host_id, public_task_id),
  UNIQUE(host_id, turn_id),
  FOREIGN KEY(host_id, public_context_id) REFERENCES agent_ops.agent_a2a_context_alias_t(host_id, public_context_id) ON DELETE CASCADE,
  FOREIGN KEY(host_id, turn_id) REFERENCES agent_ops.agent_turn_t(host_id, turn_id) ON DELETE CASCADE,
  CHECK (policy_digest ~ '^sha256:[0-9a-f]{64}$'),
  CHECK (state IN ('SUBMITTED','WORKING','INPUT_REQUIRED','AUTH_REQUIRED','COMPLETED','FAILED','CANCELED','REJECTED'))
);

CREATE TABLE agent_ops.agent_a2a_artifact_t (
  host_id UUID NOT NULL,
  artifact_id UUID NOT NULL,
  public_task_id VARCHAR(256) NOT NULL,
  logical_name VARCHAR(256) NOT NULL,
  media_type VARCHAR(256) NOT NULL,
  size_bytes BIGINT NOT NULL,
  content_digest VARCHAR(80) NOT NULL,
  object_reference VARCHAR(2048) NOT NULL,
  visibility VARCHAR(32) NOT NULL,
  retain_until_ts TIMESTAMPTZ NOT NULL,
  legal_hold BOOLEAN NOT NULL DEFAULT FALSE,
  deletion_state VARCHAR(32) NOT NULL DEFAULT 'RETAINED',
  deletion_evidence JSONB,
  created_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY(host_id, artifact_id),
  UNIQUE(host_id, public_task_id, logical_name),
  FOREIGN KEY(host_id, public_task_id) REFERENCES agent_ops.agent_a2a_task_alias_t(host_id, public_task_id) ON DELETE RESTRICT,
  CHECK (size_bytes >= 0),
  CHECK (content_digest ~ '^sha256:[0-9a-f]{64}$'),
  CHECK (object_reference !~ '^(https?|file)://'),
  CHECK (visibility IN ('OWNER','AUTHORIZED_CALLER','TENANT_POLICY'))
);

CREATE INDEX agent_a2a_context_expiry_idx
  ON agent_ops.agent_a2a_context_alias_t(expires_ts);
CREATE INDEX agent_a2a_artifact_retention_idx
  ON agent_ops.agent_a2a_artifact_t(retain_until_ts)
  WHERE legal_hold=FALSE AND deletion_state='RETAINED';

GRANT SELECT, INSERT, UPDATE, DELETE ON
  agent_ops.agent_a2a_context_alias_t,
  agent_ops.agent_a2a_task_alias_t,
  agent_ops.agent_a2a_artifact_t
TO operations_agent_runtime;
