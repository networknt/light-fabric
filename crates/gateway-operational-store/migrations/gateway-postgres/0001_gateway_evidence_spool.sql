-- Bounded edge evidence owned by light-gateway. This schema intentionally has
-- no application task/session state and stores no request or response content.

SET search_path TO gateway_ops, pg_catalog;

CREATE TABLE gateway_ops.gateway_evidence_quota_t (
  host_id UUID PRIMARY KEY,
  maximum_pending_records BIGINT NOT NULL,
  maximum_pending_bytes BIGINT NOT NULL,
  pending_records BIGINT NOT NULL DEFAULT 0,
  pending_bytes BIGINT NOT NULL DEFAULT 0,
  dropped_optional_records BIGINT NOT NULL DEFAULT 0,
  updated_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CHECK (maximum_pending_records > 0),
  CHECK (maximum_pending_bytes > 0),
  CHECK (pending_records >= 0 AND pending_records <= maximum_pending_records),
  CHECK (pending_bytes >= 0 AND pending_bytes <= maximum_pending_bytes),
  CHECK (dropped_optional_records >= 0)
);

CREATE TABLE gateway_ops.gateway_evidence_spool_t (
  host_id UUID NOT NULL,
  event_id UUID NOT NULL,
  gateway_instance VARCHAR(256) NOT NULL,
  event_class VARCHAR(32) NOT NULL,
  event_type VARCHAR(128) NOT NULL,
  method VARCHAR(16) NOT NULL,
  endpoint VARCHAR(1024) NOT NULL,
  status_code INTEGER NOT NULL,
  duration_micros BIGINT NOT NULL,
  request_bytes BIGINT NOT NULL DEFAULT 0,
  response_bytes BIGINT NOT NULL DEFAULT 0,
  correlation_digest VARCHAR(71),
  principal_digest VARCHAR(71),
  policy_digest VARCHAR(71),
  handler_digest VARCHAR(71),
  evidence_digest VARCHAR(71) NOT NULL,
  record_bytes INTEGER NOT NULL,
  state VARCHAR(24) NOT NULL DEFAULT 'PENDING',
  attempt BIGINT NOT NULL DEFAULT 0,
  next_attempt_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  leased_by VARCHAR(256),
  lease_expires_ts TIMESTAMPTZ,
  last_error_code VARCHAR(128),
  created_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  delivered_ts TIMESTAMPTZ,
  PRIMARY KEY(host_id, event_id),
  CHECK (event_class IN ('REQUIRED_AUDIT','TRAFFIC')),
  CHECK (method ~ '^[A-Z][A-Z0-9_-]{0,15}$'),
  CHECK (status_code BETWEEN 100 AND 599),
  CHECK (duration_micros >= 0),
  CHECK (request_bytes >= 0 AND response_bytes >= 0),
  CHECK (correlation_digest IS NULL OR correlation_digest ~ '^sha256:[0-9a-f]{64}$'),
  CHECK (principal_digest IS NULL OR principal_digest ~ '^sha256:[0-9a-f]{64}$'),
  CHECK (policy_digest IS NULL OR policy_digest ~ '^sha256:[0-9a-f]{64}$'),
  CHECK (handler_digest IS NULL OR handler_digest ~ '^sha256:[0-9a-f]{64}$'),
  CHECK (evidence_digest ~ '^sha256:[0-9a-f]{64}$'),
  CHECK (record_bytes BETWEEN 1 AND 16384),
  CHECK (state IN ('PENDING','PUBLISHING','DELIVERED','DEAD_LETTER')),
  CHECK (attempt >= 0),
  CHECK (endpoint !~ '[?]')
);

CREATE INDEX gateway_evidence_publish_idx
  ON gateway_ops.gateway_evidence_spool_t(host_id, next_attempt_ts, created_ts)
  WHERE state IN ('PENDING','PUBLISHING');
CREATE INDEX gateway_evidence_retention_idx
  ON gateway_ops.gateway_evidence_spool_t(host_id, delivered_ts)
  WHERE state='DELIVERED';

GRANT USAGE ON SCHEMA gateway_ops TO operations_gateway_runtime;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA gateway_ops
  TO operations_gateway_runtime;
ALTER DEFAULT PRIVILEGES FOR ROLE operations_gateway_migrator IN SCHEMA gateway_ops
  GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO operations_gateway_runtime;
