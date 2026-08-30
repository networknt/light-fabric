-- Artifact bytes live in an approved object store. PostgreSQL retains only
-- immutable identity/digest metadata and lifecycle evidence.

SET search_path TO artifact_ops, pg_catalog;

CREATE TABLE artifact_ops.artifact_metadata_t (
  host_id UUID NOT NULL,
  artifact_id UUID NOT NULL,
  owner_service VARCHAR(256) NOT NULL,
  owner_kind VARCHAR(64) NOT NULL,
  owner_id VARCHAR(512) NOT NULL,
  logical_name VARCHAR(256) NOT NULL,
  media_type VARCHAR(256) NOT NULL,
  size_bytes BIGINT NOT NULL,
  content_digest VARCHAR(71) NOT NULL,
  object_reference VARCHAR(2048) NOT NULL,
  visibility VARCHAR(32) NOT NULL,
  scan_state VARCHAR(24) NOT NULL DEFAULT 'PENDING',
  scan_profile_id VARCHAR(256),
  scan_evidence_digest VARCHAR(71),
  retain_until_ts TIMESTAMPTZ NOT NULL,
  legal_hold BOOLEAN NOT NULL DEFAULT FALSE,
  lifecycle_state VARCHAR(24) NOT NULL DEFAULT 'RETAINED',
  tombstone_digest VARCHAR(71),
  created_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY(host_id, artifact_id),
  UNIQUE(host_id, owner_service, owner_kind, owner_id, logical_name),
  CHECK (owner_kind IN ('TASK','SESSION','TURN','PROCESS','EXECUTION','CONTEXT')),
  CHECK (size_bytes >= 0),
  CHECK (content_digest ~ '^sha256:[0-9a-f]{64}$'),
  CHECK (object_reference !~ '^(https?|file)://'),
  CHECK (object_reference !~ '(^|/)\.\.(/|$)'),
  CHECK (visibility IN ('OWNER','AUTHORIZED_CALLER','TENANT_POLICY')),
  CHECK (scan_state IN ('PENDING','CLEAN','REJECTED','ERROR')),
  CHECK (scan_evidence_digest IS NULL OR scan_evidence_digest ~ '^sha256:[0-9a-f]{64}$'),
  CHECK (lifecycle_state IN ('RETAINED','DELETE_PENDING','DELETING','TOMBSTONED','DELETE_FAILED')),
  CHECK (tombstone_digest IS NULL OR tombstone_digest ~ '^sha256:[0-9a-f]{64}$')
);

CREATE TABLE artifact_ops.artifact_relationship_t (
  host_id UUID NOT NULL,
  artifact_id UUID NOT NULL,
  relationship_kind VARCHAR(32) NOT NULL,
  related_service VARCHAR(256) NOT NULL,
  related_id VARCHAR(512) NOT NULL,
  created_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY(host_id, artifact_id, relationship_kind, related_service, related_id),
  FOREIGN KEY(host_id, artifact_id) REFERENCES artifact_ops.artifact_metadata_t(host_id, artifact_id) ON DELETE RESTRICT,
  CHECK (relationship_kind IN ('TASK','SESSION','TURN','PROCESS','EXECUTION','CONTEXT'))
);

CREATE TABLE artifact_ops.artifact_hold_t (
  host_id UUID NOT NULL,
  artifact_id UUID NOT NULL,
  hold_id UUID NOT NULL,
  reason_code VARCHAR(128) NOT NULL,
  active BOOLEAN NOT NULL DEFAULT TRUE,
  created_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  released_ts TIMESTAMPTZ,
  PRIMARY KEY(host_id, artifact_id, hold_id),
  FOREIGN KEY(host_id, artifact_id) REFERENCES artifact_ops.artifact_metadata_t(host_id, artifact_id) ON DELETE RESTRICT,
  CHECK ((active AND released_ts IS NULL) OR (NOT active AND released_ts IS NOT NULL))
);

CREATE TABLE artifact_ops.artifact_event_t (
  host_id UUID NOT NULL,
  artifact_id UUID NOT NULL,
  sequence_no BIGINT NOT NULL,
  event_type VARCHAR(64) NOT NULL,
  evidence_digest VARCHAR(71) NOT NULL,
  event_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY(host_id, artifact_id, sequence_no),
  FOREIGN KEY(host_id, artifact_id) REFERENCES artifact_ops.artifact_metadata_t(host_id, artifact_id) ON DELETE RESTRICT,
  CHECK (evidence_digest ~ '^sha256:[0-9a-f]{64}$')
);

CREATE FUNCTION artifact_ops.protect_artifact_identity() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF ROW(NEW.host_id,NEW.artifact_id,NEW.owner_service,NEW.owner_kind,NEW.owner_id,
         NEW.logical_name,NEW.media_type,NEW.size_bytes,NEW.content_digest,
         NEW.object_reference,NEW.visibility,NEW.created_ts)
     IS DISTINCT FROM
     ROW(OLD.host_id,OLD.artifact_id,OLD.owner_service,OLD.owner_kind,OLD.owner_id,
         OLD.logical_name,OLD.media_type,OLD.size_bytes,OLD.content_digest,
         OLD.object_reference,OLD.visibility,OLD.created_ts) THEN
    RAISE EXCEPTION 'artifact identity and content evidence are immutable';
  END IF;
  RETURN NEW;
END
$$;

CREATE TRIGGER protect_artifact_identity_trg
BEFORE UPDATE ON artifact_ops.artifact_metadata_t
FOR EACH ROW EXECUTE FUNCTION artifact_ops.protect_artifact_identity();

CREATE INDEX artifact_retention_idx
  ON artifact_ops.artifact_metadata_t(host_id, retain_until_ts)
  WHERE legal_hold=FALSE AND lifecycle_state='RETAINED';
CREATE INDEX artifact_owner_idx
  ON artifact_ops.artifact_metadata_t(host_id, owner_service, owner_kind, owner_id);
CREATE INDEX artifact_hold_active_idx
  ON artifact_ops.artifact_hold_t(host_id, artifact_id) WHERE active;

GRANT USAGE ON SCHEMA artifact_ops TO operations_artifact_runtime;
GRANT SELECT, INSERT, UPDATE ON ALL TABLES IN SCHEMA artifact_ops TO operations_artifact_runtime;
ALTER DEFAULT PRIVILEGES FOR ROLE operations_artifact_migrator IN SCHEMA artifact_ops
  GRANT SELECT, INSERT, UPDATE ON TABLES TO operations_artifact_runtime;
