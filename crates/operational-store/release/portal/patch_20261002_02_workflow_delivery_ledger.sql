-- Portal-owned durable command/delivery authority; requires accepted W2 public schema.
-- No operational migration, activation, historical backfill or inferred delivery intent.
BEGIN;
SET LOCAL search_path TO public, pg_catalog;

CREATE TABLE public.workflow_command_receipt_t (
    host_id uuid NOT NULL,
    command_id uuid NOT NULL,
    actor_id uuid NOT NULL,
    command_type text NOT NULL,
    command_key varchar(128) NOT NULL CHECK (length(command_key) > 0),
    request_text text NOT NULL,
    request_digest varchar(64) NOT NULL CHECK (request_digest ~ '^[0-9a-f]{64}$'),
    acceptance jsonb NOT NULL CHECK (jsonb_typeof(acceptance) = 'object'),
    accepted_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (host_id, command_id),
    UNIQUE (host_id, actor_id, command_type, command_key)
);

CREATE TABLE public.workflow_delivery_intent_t (
    host_id uuid NOT NULL,
    intent_id uuid NOT NULL,
    command_id uuid NOT NULL,
    event_id uuid NOT NULL,
    wf_def_id uuid NOT NULL,
    sync_kind varchar(16) NOT NULL CHECK (sync_kind IN ('definition', 'grants')),
    source_revision bigint NOT NULL CHECK (source_revision > 0),
    definition_revision bigint NOT NULL CHECK (definition_revision > 0),
    expression_profile varchar(64) NOT NULL CHECK (expression_profile = 'cel-workflow-v2'),
    authored_content_digest varchar(64) NOT NULL CHECK (authored_content_digest ~ '^[0-9a-f]{64}$'),
    definition_digest varchar(71) NOT NULL CHECK (definition_digest ~ '^sha256:[0-9a-f]{64}$'),
    grant_set_digest varchar(71),
    actor_id uuid NOT NULL,
    payload jsonb NOT NULL CHECK (jsonb_typeof(payload) = 'object'),
    payload_digest varchar(64) NOT NULL CHECK (payload_digest ~ '^[0-9a-f]{64}$'),
    state varchar(16) NOT NULL DEFAULT 'pending'
        CHECK (state IN ('pending', 'claimed', 'completed', 'blocked', 'superseded')),
    generation bigint NOT NULL DEFAULT 0 CHECK (generation >= 0),
    lease_token uuid,
    lease_until timestamptz,
    attempts integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    receipt jsonb,
    receipt_digest varchar(64),
    error_code text,
    created_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (host_id, intent_id),
    FOREIGN KEY (host_id, command_id) REFERENCES public.workflow_command_receipt_t(host_id, command_id),
    UNIQUE (host_id, command_id, event_id, wf_def_id, sync_kind),
    UNIQUE (host_id, wf_def_id, sync_kind, source_revision),
    CHECK ((payload->>'hostId' = host_id::text AND payload->>'wfDefId' = wf_def_id::text
        AND payload->'sourceRevision' = to_jsonb(source_revision) AND payload->>'actor' = actor_id::text) IS TRUE),
    CHECK ((sync_kind = 'definition' AND grant_set_digest IS NULL)
        OR (sync_kind = 'grants' AND grant_set_digest IS NOT NULL AND grant_set_digest ~ '^sha256:[0-9a-f]{64}$')),
    CHECK ((state = 'claimed' AND lease_token IS NOT NULL AND lease_until IS NOT NULL)
        OR (state <> 'claimed' AND lease_token IS NULL AND lease_until IS NULL)),
    CHECK (((state = 'completed' AND receipt IS NOT NULL AND jsonb_typeof(receipt) = 'object'
            AND receipt_digest IS NOT NULL AND receipt_digest ~ '^[0-9a-f]{64}$'
            AND receipt->>'wfDefId' = wf_def_id::text AND receipt->'appliedRevision' = to_jsonb(source_revision)
            AND ((sync_kind='definition' AND receipt->>'result' IN ('saved','unchanged') AND receipt->>'definitionDigest'=definition_digest)
                OR (sync_kind='grants' AND receipt->>'result' IN ('synced','unchanged') AND receipt->>'grantSetDigest'=grant_set_digest)))
        OR (state <> 'completed' AND receipt IS NULL AND receipt_digest IS NULL)) IS TRUE)
);
CREATE INDEX workflow_delivery_claim_idx ON public.workflow_delivery_intent_t
    (host_id, wf_def_id, sync_kind, source_revision, state);

-- Reconstructible exact target/payload view. Its existence never grants v2 delivery authority.
CREATE TABLE public.workflow_sync_target_t (
    host_id uuid NOT NULL,
    wf_def_id uuid NOT NULL,
    sync_kind varchar(16) NOT NULL CHECK (sync_kind IN ('definition','grants')),
    source_revision bigint NOT NULL CHECK (source_revision > 0),
    definition_revision bigint NOT NULL CHECK (definition_revision > 0),
    expression_profile varchar(64) NOT NULL CHECK (expression_profile IN ('cel-workflow-v1','cel-workflow-v2')),
    definition_source text NOT NULL,
    definition_digest varchar(71) NOT NULL CHECK (definition_digest ~ '^sha256:[0-9a-f]{64}$'),
    payload jsonb NOT NULL CHECK (jsonb_typeof(payload)='object'),
    CHECK ((payload->>'hostId'=host_id::text AND payload->>'wfDefId'=wf_def_id::text
        AND payload->'sourceRevision'=to_jsonb(source_revision)) IS TRUE),
    PRIMARY KEY (host_id,wf_def_id,sync_kind,source_revision)
);

-- Explicit v2 user-start retry identity. This is not an operational acceptance record.
CREATE TABLE public.workflow_start_request_t (
    host_id uuid NOT NULL,
    actor_id uuid NOT NULL,
    command_key varchar(128) NOT NULL CHECK (length(command_key) > 0),
    request_text text NOT NULL,
    request_digest varchar(64) NOT NULL CHECK (request_digest ~ '^[0-9a-f]{64}$'),
    wf_def_id uuid NOT NULL,
    definition_digest varchar(71) NOT NULL CHECK (definition_digest ~ '^sha256:[0-9a-f]{64}$'),
    outbound_request jsonb NOT NULL CHECK (jsonb_typeof(outbound_request) = 'object'),
    receipt jsonb,
    created_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CHECK ((outbound_request->>'workflowDefinitionId'=wf_def_id::text
        AND outbound_request->>'idempotencyKey'=command_key AND outbound_request->>'expectedDefinitionDigest'=definition_digest) IS TRUE),
    CHECK ((receipt IS NULL OR (jsonb_typeof(receipt)='object' AND receipt->'accepted'='true'::jsonb
        AND receipt->>'workflowDefinitionId'=wf_def_id::text)) IS TRUE),
    PRIMARY KEY (host_id, actor_id, command_key)
);

CREATE FUNCTION public.workflow_durable_evidence_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP IN ('DELETE','TRUNCATE') THEN RAISE EXCEPTION 'WORKFLOW_DURABLE_EVIDENCE_RETENTION_REQUIRED'; END IF;
    IF TG_TABLE_NAME = 'workflow_command_receipt_t' THEN
        RAISE EXCEPTION 'WORKFLOW_COMMAND_RECEIPT_IMMUTABLE';
    END IF;
    IF TG_TABLE_NAME = 'workflow_start_request_t' THEN
        IF (to_jsonb(NEW) - 'receipt') IS DISTINCT FROM (to_jsonb(OLD) - 'receipt')
           OR OLD.receipt IS NOT NULL OR NEW.receipt IS NULL THEN
            RAISE EXCEPTION 'WORKFLOW_START_IDENTITY_IMMUTABLE';
        END IF;
        RETURN NEW;
    END IF;
    IF (to_jsonb(NEW) - ARRAY['state','generation','lease_token','lease_until','attempts',
                             'receipt','receipt_digest','error_code','updated_ts'])
        IS DISTINCT FROM
       (to_jsonb(OLD) - ARRAY['state','generation','lease_token','lease_until','attempts',
                             'receipt','receipt_digest','error_code','updated_ts']) THEN
        RAISE EXCEPTION 'WORKFLOW_DELIVERY_IDENTITY_IMMUTABLE';
    END IF;
    IF OLD.state IN ('completed','superseded') THEN
        RAISE EXCEPTION 'WORKFLOW_DELIVERY_TERMINAL_IMMUTABLE';
    END IF;
    IF NEW.state = 'claimed' THEN
        IF NEW.generation <> OLD.generation + 1
           OR NEW.lease_token IS NOT DISTINCT FROM OLD.lease_token
           OR NEW.attempts <> OLD.attempts + 1
           OR (OLD.state = 'claimed' AND OLD.lease_until > CURRENT_TIMESTAMP) THEN
            RAISE EXCEPTION 'WORKFLOW_DELIVERY_CLAIM_FENCE';
        END IF;
    ELSIF NEW.generation <> OLD.generation OR NEW.attempts <> OLD.attempts THEN
        RAISE EXCEPTION 'WORKFLOW_DELIVERY_GENERATION_IMMUTABLE';
    END IF;
    IF NEW.state = 'completed' AND OLD.state <> 'claimed' THEN
        RAISE EXCEPTION 'WORKFLOW_DELIVERY_RECEIPT_REQUIRES_CLAIM';
    END IF;
    IF NEW.state = 'pending' AND OLD.state NOT IN ('pending','claimed') THEN
        RAISE EXCEPTION 'WORKFLOW_DELIVERY_EXPLICIT_RETRY_REQUIRED';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER workflow_command_receipt_guard BEFORE UPDATE OR DELETE ON public.workflow_command_receipt_t
    FOR EACH ROW EXECUTE FUNCTION public.workflow_durable_evidence_guard();
CREATE TRIGGER workflow_delivery_intent_guard BEFORE UPDATE OR DELETE ON public.workflow_delivery_intent_t
    FOR EACH ROW EXECUTE FUNCTION public.workflow_durable_evidence_guard();
CREATE TRIGGER workflow_start_request_guard BEFORE UPDATE OR DELETE ON public.workflow_start_request_t
    FOR EACH ROW EXECUTE FUNCTION public.workflow_durable_evidence_guard();
CREATE TRIGGER workflow_command_receipt_truncate_guard BEFORE TRUNCATE ON public.workflow_command_receipt_t
    FOR EACH STATEMENT EXECUTE FUNCTION public.workflow_durable_evidence_guard();
CREATE TRIGGER workflow_delivery_intent_truncate_guard BEFORE TRUNCATE ON public.workflow_delivery_intent_t
    FOR EACH STATEMENT EXECUTE FUNCTION public.workflow_durable_evidence_guard();
CREATE TRIGGER workflow_start_request_truncate_guard BEFORE TRUNCATE ON public.workflow_start_request_t
    FOR EACH STATEMENT EXECUTE FUNCTION public.workflow_durable_evidence_guard();
REVOKE ALL ON public.workflow_command_receipt_t, public.workflow_delivery_intent_t,
    public.workflow_start_request_t FROM PUBLIC;
COMMENT ON TABLE public.workflow_command_receipt_t IS 'Durable exact principal-bound Portal acceptance. Excluded from event projection rebuild. Indefinite retention until separately reviewed archival policy.';
COMMENT ON TABLE public.workflow_delivery_intent_t IS 'Fresh-command v2 delivery authority. Immutable pinned identity/payload; claim generation and lease fence every receipt. No event replay backfill.';
COMMENT ON TABLE public.workflow_start_request_t IS 'Explicit same-caller v2 start retry identity; pending is not operational acceptance and never background-resends.';
COMMIT;
