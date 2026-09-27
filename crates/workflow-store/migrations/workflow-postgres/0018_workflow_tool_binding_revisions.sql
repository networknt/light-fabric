SET search_path TO workflow_ops, pg_catalog;

-- Immutable Workflow definition versions published by Portal.
CREATE TABLE IF NOT EXISTS workflow_ops.wf_definition_version_t (
    host_id uuid NOT NULL,
    wf_def_id uuid NOT NULL,
    version varchar(64) NOT NULL,
    definition text NOT NULL,
    definition_digest varchar(71) NOT NULL
        CHECK (definition_digest ~ '^sha256:[0-9a-f]{64}$'),
    schema_digest varchar(71) NOT NULL
        CHECK (schema_digest ~ '^sha256:[0-9a-f]{64}$'),
    binding_approval varchar(16) NOT NULL DEFAULT 'carryOver'
        CHECK (binding_approval IN ('carryOver','reapprove')),
    version_status varchar(16) NOT NULL DEFAULT 'active'
        CHECK (version_status IN ('active','retired')),
    published_by varchar(255) NOT NULL,
    published_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    retired_by varchar(255),
    retired_ts timestamptz,
    CHECK ((version_status = 'retired') = (retired_ts IS NOT NULL)),
    PRIMARY KEY (host_id, wf_def_id, version),
    FOREIGN KEY (host_id, wf_def_id)
        REFERENCES workflow_ops.wf_definition_t(host_id, wf_def_id) ON DELETE RESTRICT
);

-- Existing rows are retained as inactive legacy revisions; no binding data is
-- backfilled, deduplicated, or discarded.
ALTER TABLE workflow_ops.workflow_tool_binding_t
    ADD COLUMN IF NOT EXISTS source_binding_id uuid,
    ADD COLUMN IF NOT EXISTS revision_status varchar(24) NOT NULL DEFAULT 'legacy',
    ADD COLUMN IF NOT EXISTS binding_digest varchar(71),
    ADD COLUMN IF NOT EXISTS approval_digest varchar(71),
    ADD COLUMN IF NOT EXISTS owner_user_id uuid,
    ADD COLUMN IF NOT EXISTS owner_position_id varchar(128),
    ADD COLUMN IF NOT EXISTS requested_by varchar(255),
    ADD COLUMN IF NOT EXISTS requested_ts timestamptz,
    ADD COLUMN IF NOT EXISTS approved_by varchar(255),
    ADD COLUMN IF NOT EXISTS approved_ts timestamptz,
    ADD COLUMN IF NOT EXISTS approval_basis_id uuid,
    ADD COLUMN IF NOT EXISTS cancellation_policy varchar(24) NOT NULL DEFAULT 'before-effects-only',
    ADD COLUMN IF NOT EXISTS admission_limits jsonb NOT NULL DEFAULT
        '{"maximumConcurrentRuns":20,"maximumConcurrentRunsPerUser":2,"startsPerMinute":120,"startsPerMinutePerUser":10}',
    ADD COLUMN IF NOT EXISTS caller_policy jsonb NOT NULL DEFAULT '{}',
    ADD COLUMN IF NOT EXISTS tool_annotations jsonb NOT NULL DEFAULT '{"readOnly":true,"destructive":false}';

ALTER TABLE workflow_ops.workflow_tool_binding_t
    DROP CONSTRAINT IF EXISTS workflow_tool_binding_t_host_id_tool_id_workflow_version_key;

UPDATE workflow_ops.workflow_tool_binding_t
   SET revision_status = 'legacy', active = false;

ALTER TABLE workflow_ops.workflow_tool_binding_t
    DROP CONSTRAINT IF EXISTS workflow_tool_binding_revision_status_ck,
    DROP CONSTRAINT IF EXISTS workflow_tool_binding_active_revision_ck,
    DROP CONSTRAINT IF EXISTS workflow_tool_binding_cancellation_policy_ck,
    DROP CONSTRAINT IF EXISTS workflow_tool_binding_admission_limits_ck,
    DROP CONSTRAINT IF EXISTS workflow_tool_binding_caller_policy_ck,
    DROP CONSTRAINT IF EXISTS workflow_tool_binding_tool_annotations_ck,
    DROP CONSTRAINT IF EXISTS workflow_tool_binding_binding_digest_ck,
    DROP CONSTRAINT IF EXISTS workflow_tool_binding_approval_digest_ck,
    DROP CONSTRAINT IF EXISTS workflow_tool_binding_revision_fields_ck;

ALTER TABLE workflow_ops.workflow_tool_binding_t
    ADD CONSTRAINT workflow_tool_binding_revision_status_ck
        CHECK (revision_status IN ('pendingApproval','approved','rejected','superseded','withdrawn','retired','revoked','legacy')),
    ADD CONSTRAINT workflow_tool_binding_active_revision_ck
        CHECK (NOT active OR revision_status = 'approved'),
    ADD CONSTRAINT workflow_tool_binding_cancellation_policy_ck
        CHECK (cancellation_policy IN ('before-effects-only','cooperative','disabled')),
    ADD CONSTRAINT workflow_tool_binding_admission_limits_ck
        CHECK (jsonb_typeof(admission_limits) = 'object'),
    ADD CONSTRAINT workflow_tool_binding_caller_policy_ck
        CHECK (jsonb_typeof(caller_policy) = 'object'),
    ADD CONSTRAINT workflow_tool_binding_tool_annotations_ck
        CHECK (jsonb_typeof(tool_annotations) = 'object'),
    ADD CONSTRAINT workflow_tool_binding_binding_digest_ck
        CHECK (binding_digest IS NULL OR binding_digest ~ '^sha256:[0-9a-f]{64}$'),
    ADD CONSTRAINT workflow_tool_binding_approval_digest_ck
        CHECK (approval_digest IS NULL OR approval_digest ~ '^sha256:[0-9a-f]{64}$'),
    ADD CONSTRAINT workflow_tool_binding_revision_fields_ck
        CHECK (revision_status = 'legacy' OR
            (binding_digest IS NOT NULL AND approval_digest IS NOT NULL
             AND source_binding_id IS NOT NULL AND requested_by IS NOT NULL
             AND requested_ts IS NOT NULL));

CREATE UNIQUE INDEX IF NOT EXISTS workflow_tool_binding_one_active_per_tool_uq
    ON workflow_ops.workflow_tool_binding_t(host_id, tool_id) WHERE active;
CREATE UNIQUE INDEX IF NOT EXISTS workflow_tool_binding_one_pending_per_tool_uq
    ON workflow_ops.workflow_tool_binding_t(host_id, tool_id)
    WHERE revision_status = 'pendingApproval';
CREATE INDEX IF NOT EXISTS workflow_tool_binding_revision_status_idx
    ON workflow_ops.workflow_tool_binding_t(host_id, wf_def_id, revision_status);

CREATE TABLE IF NOT EXISTS workflow_ops.workflow_tool_publication_t (
    host_id uuid NOT NULL,
    tool_id uuid NOT NULL,
    aggregate_version bigint NOT NULL DEFAULT 0 CHECK (aggregate_version >= 0),
    active_binding_id uuid,
    pending_binding_id uuid,
    updated_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (host_id, tool_id),
    FOREIGN KEY (host_id, active_binding_id)
        REFERENCES workflow_ops.workflow_tool_binding_t(host_id, binding_id),
    FOREIGN KEY (host_id, pending_binding_id)
        REFERENCES workflow_ops.workflow_tool_binding_t(host_id, binding_id)
);

CREATE TABLE IF NOT EXISTS workflow_ops.workflow_publication_operation_t (
    host_id uuid NOT NULL,
    operation_id uuid NOT NULL,
    tool_name varchar(64) NOT NULL,
    request_digest varchar(71) NOT NULL
        CHECK (request_digest ~ '^sha256:[0-9a-f]{64}$'),
    receipt jsonb NOT NULL CHECK (jsonb_typeof(receipt) = 'object'),
    created_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (host_id, operation_id)
);
CREATE INDEX IF NOT EXISTS workflow_publication_operation_created_idx
    ON workflow_ops.workflow_publication_operation_t(created_ts);

CREATE TABLE IF NOT EXISTS workflow_ops.workflow_tool_binding_decision_t (
    host_id uuid NOT NULL,
    decision_id uuid NOT NULL,
    tool_id uuid NOT NULL,
    binding_id uuid NOT NULL,
    action varchar(16) NOT NULL CHECK (action IN
        ('approve','reject','revoke','retire','supersede','withdraw','carryOver','selfApprove')),
    actor varchar(255) NOT NULL,
    comment text,
    approval_digest varchar(71) NOT NULL
        CHECK (approval_digest ~ '^sha256:[0-9a-f]{64}$'),
    decided_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    operation_id uuid,
    CHECK (action NOT IN ('reject','revoke') OR (comment IS NOT NULL AND btrim(comment) <> '')),
    PRIMARY KEY (host_id, decision_id),
    FOREIGN KEY (host_id, binding_id)
        REFERENCES workflow_ops.workflow_tool_binding_t(host_id, binding_id)
);
CREATE INDEX IF NOT EXISTS workflow_tool_binding_decision_binding_idx
    ON workflow_ops.workflow_tool_binding_decision_t(host_id, binding_id, decided_ts);

ALTER TABLE workflow_ops.workflow_action_authority_t
    ADD COLUMN IF NOT EXISTS credential_kind varchar(8) DEFAULT 'broker';
UPDATE workflow_ops.workflow_action_authority_t a
   SET credential_kind = 'long'
 WHERE EXISTS (
       SELECT 1 FROM workflow_ops.workflow_long_credential_t c
        WHERE c.binding_id = a.grant_id
   );
ALTER TABLE workflow_ops.workflow_action_authority_t
    DROP CONSTRAINT IF EXISTS workflow_action_authority_credential_kind_ck,
    ALTER COLUMN credential_kind SET NOT NULL,
    ALTER COLUMN credential_kind DROP DEFAULT,
    ADD CONSTRAINT workflow_action_authority_credential_kind_ck
        CHECK (credential_kind IN ('broker','long','invoke'));

CREATE TABLE IF NOT EXISTS workflow_ops.workflow_run_credential_t (
    host_id uuid NOT NULL,
    workflow_instance_id uuid NOT NULL,
    key_id text NOT NULL CHECK (key_id <> 'plaintext'),
    token_bytes bytea NOT NULL,
    token_exp bigint NOT NULL CHECK (token_exp > 0),
    expires_ts timestamptz NOT NULL,
    updated_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (host_id, workflow_instance_id)
);
CREATE INDEX IF NOT EXISTS workflow_run_credential_expiry_idx
    ON workflow_ops.workflow_run_credential_t(expires_ts);
GRANT SELECT, INSERT, UPDATE, DELETE ON workflow_ops.workflow_run_credential_t
    TO operations_workflow_runtime;

ALTER TABLE workflow_ops.wf_definition_t
    ADD COLUMN IF NOT EXISTS source_revision bigint NOT NULL DEFAULT 0
        CHECK (source_revision >= 0);

CREATE TABLE IF NOT EXISTS workflow_ops.workflow_definition_grant_sync_t (
    host_id uuid NOT NULL,
    wf_def_id uuid NOT NULL,
    source_revision bigint NOT NULL CHECK (source_revision >= 0),
    grant_set_digest varchar(71) NOT NULL
        CHECK (grant_set_digest ~ '^sha256:[0-9a-f]{64}$'),
    synced_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (host_id, wf_def_id),
    FOREIGN KEY (host_id, wf_def_id)
        REFERENCES workflow_ops.wf_definition_t(host_id, wf_def_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS workflow_invocation_tool_open_idx
    ON workflow_ops.workflow_invocation_t(host_id, stable_tool_ref, accepted_ts)
    WHERE state NOT IN ('COMPLETED','FAILED','CANCELLED');

CREATE OR REPLACE FUNCTION workflow_ops.workflow_claim_idempotency_v2(
    p_host_id uuid,
    p_reservation_id uuid,
    p_scope_digest character varying,
    p_idempotency_kind character varying,
    p_stable_tool_ref uuid,
    p_principal_subject character varying,
    p_end_user_subject character varying,
    p_workflow_instance_id uuid,
    p_definition_digest character varying,
    p_input_digest character varying,
    p_in_flight_until timestamp with time zone,
    p_result_replay_until timestamp with time zone
) RETURNS TABLE(
    outcome character varying,
    accepted_workflow_instance_id uuid,
    accepted_generation bigint
)
LANGUAGE plpgsql
AS $$
DECLARE
    current_row workflow_ops.workflow_invocation_idempotency_t%ROWTYPE;
    invocation_state character varying;
    invocation_terminal_ts timestamp with time zone;
    invocation_is_terminal boolean := false;
    request_matches boolean;
BEGIN
    -- Claims for one host/scope must serialize across an expired generation
    -- replacement. Without this lock, a waiter can recheck the old row after
    -- it is deactivated and observe no row at all.
    PERFORM pg_advisory_xact_lock(hashtextextended(
        'workflow_claim_idempotency_v2:' || p_host_id::text || ':' || p_scope_digest,
        0
    ));

    INSERT INTO workflow_ops.workflow_invocation_idempotency_t(
        host_id,reservation_id,scope_digest,idempotency_kind,stable_tool_ref,
        principal_subject,end_user_subject,workflow_instance_id,
        definition_digest,input_digest,in_flight_until,result_replay_until
    ) VALUES (
        p_host_id,p_reservation_id,p_scope_digest,p_idempotency_kind,p_stable_tool_ref,
        p_principal_subject,p_end_user_subject,p_workflow_instance_id,
        p_definition_digest,p_input_digest,p_in_flight_until,p_result_replay_until
    ) ON CONFLICT(host_id,scope_digest) WHERE active DO NOTHING;
    IF FOUND THEN
        RETURN QUERY SELECT 'ACCEPTED'::varchar,p_workflow_instance_id,1::bigint;
        RETURN;
    END IF;

    SELECT * INTO current_row
      FROM workflow_ops.workflow_invocation_idempotency_t
     WHERE host_id = p_host_id AND scope_digest = p_scope_digest AND active
     FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'active idempotency reservation disappeared during claim'
            USING ERRCODE = '40001';
    END IF;

    SELECT i.state, i.terminal_ts
      INTO invocation_state, invocation_terminal_ts
      FROM workflow_ops.workflow_invocation_t i
     WHERE i.host_id = p_host_id
       AND i.workflow_instance_id = current_row.workflow_instance_id;
    IF FOUND THEN
        invocation_is_terminal := invocation_state IN ('COMPLETED','FAILED','CANCELLED');
    END IF;

    request_matches := current_row.stable_tool_ref = p_stable_tool_ref
        AND current_row.principal_subject = p_principal_subject
        AND current_row.end_user_subject = p_end_user_subject
        AND current_row.definition_digest = p_definition_digest
        AND current_row.input_digest = p_input_digest;

    IF NOT invocation_is_terminal THEN
        IF request_matches THEN
            RETURN QUERY SELECT 'REPLAY'::varchar,current_row.workflow_instance_id,current_row.generation;
        ELSE
            RETURN QUERY SELECT 'CONFLICT'::varchar,current_row.workflow_instance_id,current_row.generation;
        END IF;
        RETURN;
    END IF;

    IF current_row.result_replay_until > CURRENT_TIMESTAMP THEN
        IF request_matches THEN
            RETURN QUERY SELECT 'REPLAY'::varchar,current_row.workflow_instance_id,current_row.generation;
        ELSE
            RETURN QUERY SELECT 'CONFLICT'::varchar,current_row.workflow_instance_id,current_row.generation;
        END IF;
        RETURN;
    END IF;

    UPDATE workflow_ops.workflow_invocation_idempotency_t
       SET active = false, updated_ts = CURRENT_TIMESTAMP
     WHERE host_id = p_host_id AND reservation_id = current_row.reservation_id;
    RETURN QUERY
        INSERT INTO workflow_ops.workflow_invocation_idempotency_t(
            host_id,reservation_id,scope_digest,idempotency_kind,stable_tool_ref,
            principal_subject,end_user_subject,workflow_instance_id,
            definition_digest,input_digest,generation,in_flight_until,result_replay_until
        ) VALUES (
            p_host_id,p_reservation_id,p_scope_digest,p_idempotency_kind,p_stable_tool_ref,
            p_principal_subject,p_end_user_subject,p_workflow_instance_id,
            p_definition_digest,p_input_digest,current_row.generation + 1,
            p_in_flight_until,p_result_replay_until
        ) RETURNING 'ACCEPTED'::varchar,workflow_instance_id,generation;
END
$$;

CREATE OR REPLACE FUNCTION workflow_ops.workflow_invocation_terminal_cleanup_0018()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF OLD.state IS DISTINCT FROM NEW.state
       AND NEW.state IN ('COMPLETED','FAILED','CANCELLED') THEN
        DELETE FROM workflow_ops.workflow_run_credential_t
         WHERE host_id = NEW.host_id
           AND workflow_instance_id = NEW.workflow_instance_id;

        IF NEW.binding_id IS NOT NULL THEN
            UPDATE workflow_ops.workflow_invocation_idempotency_t i
               SET in_flight_until = LEAST(i.in_flight_until, NEW.terminal_ts),
                   result_replay_until = CASE
                       WHEN NEW.state IN ('FAILED','CANCELLED') AND NEW.effect_state = 'none'
                           THEN NEW.terminal_ts
                       ELSE NEW.terminal_ts + make_interval(secs =>
                           COALESCE((b.idempotency_policy->>'resultReplayMs')::bigint, 0) / 1000.0)
                   END,
                   updated_ts = CURRENT_TIMESTAMP
              FROM workflow_ops.workflow_tool_binding_t b
             WHERE i.host_id = NEW.host_id
               AND i.workflow_instance_id = NEW.workflow_instance_id
               AND i.active
               AND b.host_id = NEW.host_id
               AND b.binding_id = NEW.binding_id;
        END IF;
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS workflow_invocation_terminal_cleanup_0018_trg
    ON workflow_ops.workflow_invocation_t;
CREATE TRIGGER workflow_invocation_terminal_cleanup_0018_trg
    AFTER UPDATE OF state ON workflow_ops.workflow_invocation_t
    FOR EACH ROW
    WHEN (NEW.state IN ('COMPLETED','FAILED','CANCELLED'))
    EXECUTE FUNCTION workflow_ops.workflow_invocation_terminal_cleanup_0018();

GRANT SELECT, INSERT, UPDATE ON
    workflow_ops.wf_definition_version_t,
    workflow_ops.workflow_tool_publication_t,
    workflow_ops.workflow_definition_grant_sync_t,
    workflow_ops.workflow_publication_operation_t
    TO operations_workflow_runtime;
GRANT DELETE ON workflow_ops.workflow_publication_operation_t
    TO operations_workflow_runtime;
GRANT SELECT, INSERT ON workflow_ops.workflow_tool_binding_decision_t
    TO operations_workflow_runtime;
GRANT EXECUTE ON FUNCTION workflow_ops.workflow_claim_idempotency_v2(
    uuid,uuid,character varying,character varying,uuid,character varying,
    character varying,uuid,character varying,character varying,timestamptz,timestamptz
) TO operations_workflow_runtime;
