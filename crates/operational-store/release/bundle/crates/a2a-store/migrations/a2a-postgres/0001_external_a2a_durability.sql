-- External A2A facade durability. Runtime business payloads remain bounded;
-- artifact bytes live in approved object storage and only immutable metadata is
-- retained here.

SET search_path TO a2a_ops, pg_catalog;

CREATE TABLE a2a_ops.a2a_context_t (
  host_id UUID NOT NULL,
  context_id UUID NOT NULL,
  public_context_id VARCHAR(256) NOT NULL,
  principal_subject VARCHAR(512) NOT NULL,
  caller_agent_ref VARCHAR(256) NOT NULL,
  target_agent_ref VARCHAR(256) NOT NULL,
  binding_id UUID NOT NULL,
  publication_id UUID NOT NULL,
  policy_digest VARCHAR(80) NOT NULL,
  audience VARCHAR(64) NOT NULL,
  state VARCHAR(32) NOT NULL DEFAULT 'ACTIVE',
  expires_ts TIMESTAMPTZ NOT NULL,
  created_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY(host_id, context_id),
  UNIQUE(host_id, public_context_id),
  CHECK (audience='light-a2a'),
  CHECK (policy_digest ~ '^sha256:[0-9a-f]{64}$'),
  CHECK (state IN ('ACTIVE','CLOSED','EXPIRED'))
);

CREATE TABLE a2a_ops.a2a_task_t (
  host_id UUID NOT NULL,
  task_id UUID NOT NULL,
  public_task_id VARCHAR(256) NOT NULL,
  context_id UUID NOT NULL,
  direction VARCHAR(16) NOT NULL,
  caller_agent_ref VARCHAR(256) NOT NULL,
  target_agent_ref VARCHAR(256) NOT NULL,
  binding_id UUID NOT NULL,
  publication_id UUID NOT NULL,
  principal_subject VARCHAR(512) NOT NULL,
  policy_digest VARCHAR(80) NOT NULL,
  idempotency_key VARCHAR(512) NOT NULL,
  request_digest VARCHAR(80) NOT NULL,
  state VARCHAR(32) NOT NULL DEFAULT 'SUBMITTED',
  remote_task_id VARCHAR(512),
  remote_context_id VARCHAR(512),
  result JSONB,
  error JSONB,
  cancel_requested_ts TIMESTAMPTZ,
  terminal_ts TIMESTAMPTZ,
  aggregate_version BIGINT NOT NULL DEFAULT 1,
  created_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY(host_id, task_id),
  UNIQUE(host_id, public_task_id),
  UNIQUE(host_id, binding_id, direction, idempotency_key),
  FOREIGN KEY(host_id, context_id) REFERENCES a2a_ops.a2a_context_t(host_id, context_id) ON DELETE RESTRICT,
  CHECK (direction IN ('INBOUND','OUTBOUND')),
  CHECK (state IN ('SUBMITTED','WORKING','INPUT_REQUIRED','AUTH_REQUIRED','COMPLETED','FAILED','CANCELED','REJECTED')),
  CHECK (policy_digest ~ '^sha256:[0-9a-f]{64}$'),
  CHECK (request_digest ~ '^sha256:[0-9a-f]{64}$'),
  CHECK (aggregate_version > 0)
);

CREATE TABLE a2a_ops.a2a_message_idempotency_t (
  host_id UUID NOT NULL,
  binding_id UUID NOT NULL,
  direction VARCHAR(16) NOT NULL,
  idempotency_key VARCHAR(512) NOT NULL,
  request_digest VARCHAR(80) NOT NULL,
  task_id UUID NOT NULL,
  replay_count BIGINT NOT NULL DEFAULT 0,
  created_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  last_replay_ts TIMESTAMPTZ,
  PRIMARY KEY(host_id, binding_id, direction, idempotency_key),
  FOREIGN KEY(host_id, task_id) REFERENCES a2a_ops.a2a_task_t(host_id, task_id) ON DELETE RESTRICT,
  CHECK (direction IN ('INBOUND','OUTBOUND')),
  CHECK (request_digest ~ '^sha256:[0-9a-f]{64}$')
);

CREATE TABLE a2a_ops.a2a_backend_correlation_t (
  host_id UUID NOT NULL,
  task_id UUID NOT NULL,
  backend_kind VARCHAR(32) NOT NULL,
  backend_binding_id UUID NOT NULL,
  opaque_correlation_id VARCHAR(1024) NOT NULL,
  status_cursor VARCHAR(1024),
  last_status VARCHAR(64),
  next_reconcile_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  reconciliation_attempt BIGINT NOT NULL DEFAULT 0,
  updated_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY(host_id, task_id),
  FOREIGN KEY(host_id, task_id) REFERENCES a2a_ops.a2a_task_t(host_id, task_id) ON DELETE CASCADE,
  CHECK (backend_kind IN ('EXTERNAL_SIDECAR','REMOTE_A2A'))
);

CREATE TABLE a2a_ops.a2a_callback_t (
  host_id UUID NOT NULL,
  callback_id UUID NOT NULL,
  task_id UUID NOT NULL,
  callback_kind VARCHAR(32) NOT NULL,
  callback_reference VARCHAR(1024) NOT NULL,
  callback_secret_ref VARCHAR(512),
  state VARCHAR(32) NOT NULL DEFAULT 'PENDING',
  attempt BIGINT NOT NULL DEFAULT 0,
  next_attempt_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  last_error_code VARCHAR(128),
  created_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY(host_id, callback_id),
  FOREIGN KEY(host_id, task_id) REFERENCES a2a_ops.a2a_task_t(host_id, task_id) ON DELETE CASCADE,
  CHECK (state IN ('PENDING','DELIVERING','DELIVERED','FAILED','DEAD_LETTER'))
);

CREATE TABLE a2a_ops.a2a_artifact_t (
  host_id UUID NOT NULL,
  artifact_id UUID NOT NULL,
  task_id UUID NOT NULL,
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
  UNIQUE(host_id, task_id, logical_name),
  FOREIGN KEY(host_id, task_id) REFERENCES a2a_ops.a2a_task_t(host_id, task_id) ON DELETE RESTRICT,
  CHECK (size_bytes >= 0),
  CHECK (content_digest ~ '^sha256:[0-9a-f]{64}$'),
  CHECK (object_reference !~ '^(https?|file)://'),
  CHECK (visibility IN ('OWNER','AUTHORIZED_CALLER','TENANT_POLICY')),
  CHECK (deletion_state IN ('RETAINED','DELETE_PENDING','DELETING','DELETED','DELETE_FAILED'))
);

CREATE TABLE a2a_ops.a2a_task_event_t (
  host_id UUID NOT NULL,
  task_id UUID NOT NULL,
  sequence_no BIGINT NOT NULL,
  event_type VARCHAR(64) NOT NULL,
  event_payload JSONB NOT NULL DEFAULT '{}'::jsonb,
  event_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY(host_id, task_id, sequence_no),
  FOREIGN KEY(host_id, task_id) REFERENCES a2a_ops.a2a_task_t(host_id, task_id) ON DELETE CASCADE
);

CREATE TABLE a2a_ops.a2a_audit_outbox_t (
  host_id UUID NOT NULL,
  event_id UUID NOT NULL,
  task_id UUID,
  event_type VARCHAR(128) NOT NULL,
  correlation_id VARCHAR(512) NOT NULL,
  redacted_payload JSONB NOT NULL,
  state VARCHAR(32) NOT NULL DEFAULT 'PENDING',
  attempt BIGINT NOT NULL DEFAULT 0,
  next_attempt_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  created_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY(host_id, event_id),
  FOREIGN KEY(host_id, task_id) REFERENCES a2a_ops.a2a_task_t(host_id, task_id) ON DELETE SET NULL,
  CHECK (state IN ('PENDING','PUBLISHING','PUBLISHED','FAILED','DEAD_LETTER'))
);

CREATE TABLE a2a_ops.a2a_delegation_replay_t (
  host_id UUID NOT NULL,
  delegation_id UUID NOT NULL,
  audience VARCHAR(64) NOT NULL,
  request_digest VARCHAR(80) NOT NULL,
  expires_ts TIMESTAMPTZ NOT NULL,
  consumed_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY(host_id, delegation_id, audience),
  CHECK (audience='light-a2a'),
  CHECK (request_digest ~ '^sha256:[0-9a-f]{64}$')
);

CREATE INDEX a2a_task_reconcile_idx ON a2a_ops.a2a_task_t(host_id,state,updated_ts)
  WHERE state NOT IN ('COMPLETED','FAILED','CANCELED','REJECTED');
CREATE INDEX a2a_backend_reconcile_idx ON a2a_ops.a2a_backend_correlation_t(next_reconcile_ts);
CREATE INDEX a2a_artifact_retention_idx ON a2a_ops.a2a_artifact_t(retain_until_ts)
  WHERE legal_hold=FALSE AND deletion_state='RETAINED';

GRANT USAGE ON SCHEMA a2a_ops TO operations_a2a_runtime;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA a2a_ops TO operations_a2a_runtime;
ALTER DEFAULT PRIVILEGES FOR ROLE operations_a2a_migrator IN SCHEMA a2a_ops
  GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO operations_a2a_runtime;
