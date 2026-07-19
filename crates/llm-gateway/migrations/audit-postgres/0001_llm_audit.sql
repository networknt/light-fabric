-- Dedicated LLM inference-audit database. This migration must not be applied
-- to Portal's control-plane database.

CREATE TABLE IF NOT EXISTS llm_audit_event_t (
    event_day date NOT NULL,
    event_id uuid NOT NULL,
    schema_version smallint NOT NULL CHECK (schema_version = 1),
    event_kind varchar(32) NOT NULL CHECK (event_kind IN (
        'request_admitted', 'attempt_started', 'attempt_finished', 'request_finished'
    )),
    request_id uuid NOT NULL,
    attempt_no integer,
    attempt_count integer,
    event_ts timestamptz NOT NULL,
    generation bigint NOT NULL CHECK (generation >= 0),
    snapshot_digest char(64) NOT NULL,
    host_id varchar(255) NOT NULL,
    public_alias varchar(255) NOT NULL,
    operation varchar(64) NOT NULL,
    status varchar(64) NOT NULL,
    category varchar(64) NOT NULL,
    deployment_id varchar(255),
    duration_ms bigint NOT NULL CHECK (duration_ms >= 0),
    content_mode varchar(32) NOT NULL CHECK (content_mode = 'metadata_only'),
    pii_profile varchar(64) NOT NULL CHECK (pii_profile = 'none'),
    principal_digest char(64) NOT NULL,
    charged_micros bigint,
    usage_complete boolean,
    ingested_ts timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (event_day, event_id),
    CHECK (attempt_no IS NULL OR attempt_no > 0),
    CHECK (attempt_count IS NULL OR attempt_count >= 0),
    CHECK (charged_micros IS NULL OR charged_micros >= 0)
) PARTITION BY RANGE (event_day);

CREATE TABLE IF NOT EXISTS llm_request_t (
    event_day date NOT NULL,
    request_id uuid NOT NULL,
    admitted_event_id uuid NOT NULL,
    finished_event_id uuid,
    host_id varchar(255) NOT NULL,
    public_alias varchar(255) NOT NULL,
    generation bigint NOT NULL CHECK (generation >= 0),
    snapshot_digest char(64) NOT NULL,
    terminal_status varchar(64),
    charged_micros bigint,
    usage_complete boolean,
    attempt_count integer CHECK (attempt_count IS NULL OR attempt_count >= 0),
    incomplete boolean NOT NULL DEFAULT true,
    PRIMARY KEY (event_day, request_id)
) PARTITION BY RANGE (event_day);

CREATE TABLE IF NOT EXISTS llm_attempt_t (
    event_day date NOT NULL,
    request_id uuid NOT NULL,
    attempt_no integer NOT NULL CHECK (attempt_no > 0),
    started_event_id uuid NOT NULL,
    deployment_id varchar(255) NOT NULL,
    finished_event_id uuid,
    terminal_status varchar(64),
    incomplete boolean NOT NULL DEFAULT true,
    PRIMARY KEY (event_day, request_id, attempt_no)
) PARTITION BY RANGE (event_day);

CREATE TABLE IF NOT EXISTS llm_content_object_t (
    event_day date NOT NULL,
    content_object_id uuid NOT NULL,
    request_id uuid NOT NULL,
    object_kind varchar(32) NOT NULL,
    storage_reference varchar(1024) NOT NULL,
    content_digest char(64) NOT NULL,
    encryption_profile varchar(128) NOT NULL,
    created_ts timestamptz NOT NULL,
    PRIMARY KEY (event_day, content_object_id)
) PARTITION BY RANGE (event_day);

CREATE TABLE IF NOT EXISTS llm_dataset_export_t (
    export_id uuid PRIMARY KEY,
    requested_by varchar(255) NOT NULL,
    selection_digest char(64) NOT NULL,
    status varchar(32) NOT NULL,
    created_ts timestamptz NOT NULL,
    completed_ts timestamptz
);

-- A deployment creates future daily partitions ahead of ingestion. This
-- deterministic bootstrap partition exists only so a fresh schema can pass
-- its smoke test without silently accepting arbitrary dates.
CREATE TABLE IF NOT EXISTS llm_audit_event_default_t
    PARTITION OF llm_audit_event_t DEFAULT;
CREATE TABLE IF NOT EXISTS llm_request_default_t
    PARTITION OF llm_request_t DEFAULT;
CREATE TABLE IF NOT EXISTS llm_attempt_default_t
    PARTITION OF llm_attempt_t DEFAULT;
CREATE TABLE IF NOT EXISTS llm_content_object_default_t
    PARTITION OF llm_content_object_t DEFAULT;

CREATE INDEX IF NOT EXISTS llm_audit_event_request_idx
    ON llm_audit_event_t (request_id, attempt_no, event_ts);
CREATE INDEX IF NOT EXISTS llm_audit_event_unfinished_idx
    ON llm_audit_event_t (event_kind, event_ts)
    WHERE event_kind = 'attempt_started';

COMMENT ON TABLE llm_audit_event_t IS
    'Metadata-only, idempotent LLM audit ingest; no prompt, completion, tool arguments, credentials, raw provider errors, or reversible PII.';

-- NOLOGIN group roles keep privileges separate from credential lifecycle.
-- Deployment automation creates distinct LOGIN roles and grants exactly one
-- of these groups; Portal credentials receive none of them.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'llm_audit_gateway_ingest') THEN
        CREATE ROLE llm_audit_gateway_ingest NOLOGIN;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'llm_audit_auditor_read') THEN
        CREATE ROLE llm_audit_auditor_read NOLOGIN;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'llm_audit_retention') THEN
        CREATE ROLE llm_audit_retention NOLOGIN;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'llm_audit_dataset_export') THEN
        CREATE ROLE llm_audit_dataset_export NOLOGIN;
    END IF;
END
$$;

GRANT INSERT, UPDATE ON llm_audit_event_t, llm_request_t, llm_attempt_t
    TO llm_audit_gateway_ingest;
GRANT SELECT ON llm_audit_event_t, llm_request_t, llm_attempt_t,
    llm_content_object_t, llm_dataset_export_t TO llm_audit_auditor_read;
GRANT SELECT, DELETE ON llm_audit_event_t, llm_request_t, llm_attempt_t,
    llm_content_object_t, llm_dataset_export_t TO llm_audit_retention;
GRANT SELECT ON llm_request_t, llm_attempt_t, llm_content_object_t
    TO llm_audit_dataset_export;
GRANT INSERT, UPDATE ON llm_dataset_export_t TO llm_audit_dataset_export;
