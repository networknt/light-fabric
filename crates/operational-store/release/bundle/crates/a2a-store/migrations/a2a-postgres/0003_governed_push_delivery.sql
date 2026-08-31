BEGIN;

CREATE TABLE IF NOT EXISTS a2a_ops.a2a_push_config_t (
  host_id UUID NOT NULL,
  config_id UUID NOT NULL,
  task_id UUID NOT NULL,
  binding_id UUID NOT NULL,
  principal_subject VARCHAR(512) NOT NULL,
  callback_registration_id UUID NOT NULL,
  callback_url_digest VARCHAR(71) NOT NULL,
  created_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY(host_id, config_id),
  FOREIGN KEY(host_id, task_id) REFERENCES a2a_ops.a2a_task_t(host_id, task_id) ON DELETE CASCADE,
  CHECK (callback_url_digest ~ '^sha256:[0-9a-f]{64}$')
);

CREATE INDEX IF NOT EXISTS a2a_push_config_task_idx
  ON a2a_ops.a2a_push_config_t(host_id, task_id, created_ts, config_id);

CREATE TABLE IF NOT EXISTS a2a_ops.a2a_push_delivery_t (
  host_id UUID NOT NULL,
  delivery_id UUID NOT NULL,
  config_id UUID NOT NULL,
  task_id UUID NOT NULL,
  delivery_nonce UUID NOT NULL,
  payload JSONB NOT NULL,
  payload_digest VARCHAR(71) NOT NULL,
  state VARCHAR(32) NOT NULL DEFAULT 'PENDING',
  attempt BIGINT NOT NULL DEFAULT 0,
  maximum_attempts BIGINT NOT NULL,
  next_attempt_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  lease_owner VARCHAR(256),
  lease_until_ts TIMESTAMPTZ,
  last_http_status INTEGER,
  last_error_code VARCHAR(128),
  delivered_ts TIMESTAMPTZ,
  dead_letter_ts TIMESTAMPTZ,
  created_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY(host_id, delivery_id),
  UNIQUE(host_id, delivery_nonce),
  FOREIGN KEY(host_id, config_id) REFERENCES a2a_ops.a2a_push_config_t(host_id, config_id) ON DELETE CASCADE,
  FOREIGN KEY(host_id, task_id) REFERENCES a2a_ops.a2a_task_t(host_id, task_id) ON DELETE CASCADE,
  CHECK (jsonb_typeof(payload)='object'),
  CHECK (payload_digest ~ '^sha256:[0-9a-f]{64}$'),
  CHECK (state IN ('PENDING','DELIVERING','DELIVERED','DEAD_LETTER')),
  CHECK (attempt>=0 AND maximum_attempts>0 AND attempt<=maximum_attempts),
  CHECK ((state='DELIVERING')=(lease_owner IS NOT NULL AND lease_until_ts IS NOT NULL)),
  CHECK ((state='DELIVERED')=(delivered_ts IS NOT NULL)),
  CHECK ((state='DEAD_LETTER')=(dead_letter_ts IS NOT NULL))
);

CREATE INDEX IF NOT EXISTS a2a_push_delivery_due_idx
  ON a2a_ops.a2a_push_delivery_t(host_id, next_attempt_ts, delivery_id)
  WHERE state IN ('PENDING','DELIVERING');

GRANT SELECT, INSERT, UPDATE, DELETE ON a2a_ops.a2a_push_config_t TO operations_a2a_runtime;
GRANT SELECT, INSERT, UPDATE, DELETE ON a2a_ops.a2a_push_delivery_t TO operations_a2a_runtime;

COMMIT;
