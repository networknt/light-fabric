-- Append-only, redacted tenant audit evidence. High-volume traffic belongs in
-- the configured analytical sink and is not retained here by default.

SET search_path TO audit_ops, pg_catalog;

CREATE FUNCTION audit_ops.audit_payload_is_redacted(payload JSONB) RETURNS BOOLEAN
LANGUAGE plpgsql IMMUTABLE PARALLEL SAFE AS $$
DECLARE
  field_name TEXT;
  field_value JSONB;
  normalized_name TEXT;
BEGIN
  IF jsonb_typeof(payload) = 'object' THEN
    FOR field_name, field_value IN SELECT * FROM jsonb_each(payload) LOOP
      normalized_name := lower(regexp_replace(field_name, '[^a-zA-Z0-9-]', '', 'g'));
      IF normalized_name = ANY(ARRAY[
        'authorization','cookie','set-cookie','password','secret','token','credential',
        'prompt','message','content','arguments','request','requestbody','response',
        'responsebody','body','artifactbytes'
      ]) OR NOT audit_ops.audit_payload_is_redacted(field_value) THEN
        RETURN FALSE;
      END IF;
    END LOOP;
  ELSIF jsonb_typeof(payload) = 'array' THEN
    FOR field_value IN SELECT * FROM jsonb_array_elements(payload) LOOP
      IF NOT audit_ops.audit_payload_is_redacted(field_value) THEN
        RETURN FALSE;
      END IF;
    END LOOP;
  END IF;
  RETURN TRUE;
END
$$;

CREATE TABLE audit_ops.audit_record_t (
  host_id UUID NOT NULL,
  audit_id UUID NOT NULL,
  source_service VARCHAR(256) NOT NULL,
  source_instance VARCHAR(256) NOT NULL,
  event_type VARCHAR(128) NOT NULL,
  event_class VARCHAR(32) NOT NULL,
  actor_digest VARCHAR(71),
  subject_kind VARCHAR(64),
  subject_digest VARCHAR(71),
  correlation_digest VARCHAR(71),
  policy_digest VARCHAR(71),
  redacted_payload JSONB NOT NULL DEFAULT '{}'::jsonb,
  evidence_digest VARCHAR(71) NOT NULL,
  occurred_ts TIMESTAMPTZ NOT NULL,
  retain_until_ts TIMESTAMPTZ NOT NULL,
  legal_hold BOOLEAN NOT NULL DEFAULT FALSE,
  erasure_state VARCHAR(24) NOT NULL DEFAULT 'RETAINED',
  erasure_evidence_digest VARCHAR(71),
  created_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY(host_id, audit_id),
  UNIQUE(host_id, evidence_digest),
  CHECK (event_class IN ('SECURITY','ACCOUNTING','APPROVAL','ARTIFACT','DELETION','OPERATOR')),
  CHECK (actor_digest IS NULL OR actor_digest ~ '^sha256:[0-9a-f]{64}$'),
  CHECK (subject_digest IS NULL OR subject_digest ~ '^sha256:[0-9a-f]{64}$'),
  CHECK (correlation_digest IS NULL OR correlation_digest ~ '^sha256:[0-9a-f]{64}$'),
  CHECK (policy_digest IS NULL OR policy_digest ~ '^sha256:[0-9a-f]{64}$'),
  CHECK (evidence_digest ~ '^sha256:[0-9a-f]{64}$'),
  CHECK (erasure_evidence_digest IS NULL OR erasure_evidence_digest ~ '^sha256:[0-9a-f]{64}$'),
  CHECK (erasure_state IN ('RETAINED','ERASURE_PENDING','TOMBSTONED')),
  CHECK (jsonb_typeof(redacted_payload)='object'),
  CHECK (audit_ops.audit_payload_is_redacted(redacted_payload))
);

CREATE TABLE audit_ops.audit_delivery_t (
  host_id UUID NOT NULL,
  audit_id UUID NOT NULL,
  sink_profile_id VARCHAR(256) NOT NULL,
  state VARCHAR(24) NOT NULL DEFAULT 'PENDING',
  attempt BIGINT NOT NULL DEFAULT 0,
  next_attempt_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  last_error_code VARCHAR(128),
  delivered_ts TIMESTAMPTZ,
  updated_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY(host_id, audit_id, sink_profile_id),
  FOREIGN KEY(host_id, audit_id) REFERENCES audit_ops.audit_record_t(host_id, audit_id) ON DELETE RESTRICT,
  CHECK (state IN ('PENDING','PUBLISHING','DELIVERED','DEAD_LETTER')),
  CHECK (attempt >= 0)
);

CREATE TABLE audit_ops.audit_hold_t (
  host_id UUID NOT NULL,
  hold_id UUID NOT NULL,
  subject_kind VARCHAR(64) NOT NULL,
  subject_digest VARCHAR(71) NOT NULL,
  reason_code VARCHAR(128) NOT NULL,
  active BOOLEAN NOT NULL DEFAULT TRUE,
  created_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  released_ts TIMESTAMPTZ,
  PRIMARY KEY(host_id, hold_id),
  CHECK (subject_digest ~ '^sha256:[0-9a-f]{64}$'),
  CHECK ((active AND released_ts IS NULL) OR (NOT active AND released_ts IS NOT NULL))
);

CREATE FUNCTION audit_ops.protect_audit_record_core() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF ROW(NEW.host_id,NEW.audit_id,NEW.source_service,NEW.source_instance,
         NEW.event_type,NEW.event_class,NEW.actor_digest,NEW.subject_kind,
         NEW.subject_digest,NEW.correlation_digest,NEW.policy_digest,
         NEW.redacted_payload,NEW.evidence_digest,NEW.occurred_ts,NEW.created_ts)
     IS DISTINCT FROM
     ROW(OLD.host_id,OLD.audit_id,OLD.source_service,OLD.source_instance,
         OLD.event_type,OLD.event_class,OLD.actor_digest,OLD.subject_kind,
         OLD.subject_digest,OLD.correlation_digest,OLD.policy_digest,
         OLD.redacted_payload,OLD.evidence_digest,OLD.occurred_ts,OLD.created_ts) THEN
    RAISE EXCEPTION 'audit record core is immutable';
  END IF;
  RETURN NEW;
END
$$;

CREATE TRIGGER protect_audit_record_core_trg
BEFORE UPDATE ON audit_ops.audit_record_t
FOR EACH ROW EXECUTE FUNCTION audit_ops.protect_audit_record_core();

CREATE INDEX audit_record_retention_idx
  ON audit_ops.audit_record_t(host_id, retain_until_ts)
  WHERE legal_hold=FALSE AND erasure_state='RETAINED';
CREATE INDEX audit_delivery_publish_idx
  ON audit_ops.audit_delivery_t(host_id, next_attempt_ts)
  WHERE state IN ('PENDING','PUBLISHING');
CREATE INDEX audit_hold_subject_idx
  ON audit_ops.audit_hold_t(host_id, subject_kind, subject_digest)
  WHERE active;

GRANT USAGE ON SCHEMA audit_ops TO operations_audit_publisher;
GRANT SELECT, INSERT, UPDATE ON ALL TABLES IN SCHEMA audit_ops TO operations_audit_publisher;
GRANT USAGE, SELECT, UPDATE ON ALL SEQUENCES IN SCHEMA audit_ops TO operations_audit_publisher;
ALTER DEFAULT PRIVILEGES FOR ROLE operations_audit_migrator IN SCHEMA audit_ops
  GRANT SELECT, INSERT, UPDATE ON TABLES TO operations_audit_publisher;
