-- Phase 3: authoritative shared execution state in operations.execution_ops.
-- The schema already exists from the Phase 1 operational metadata bundle.
-- This migration is intentionally empty-data only; development deployments
-- reset and reseed instead of copying rows from Config Server.


-- Dumped from database version 17.10
-- Dumped by pg_dump version 17.10

SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET transaction_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

SET default_tablespace = '';

SET default_table_access_method = heap;

GRANT SELECT ON operational_meta.operational_schema_migration_t
    TO operations_execution_runtime;

SET ROLE operations_execution_migrator;

CREATE FUNCTION execution_ops.execution_runtime_audit_append_only() RETURNS trigger
LANGUAGE plpgsql
AS $function$
BEGIN
    RAISE EXCEPTION 'execution_runtime_audit_t is append-only';
END
$function$;

CREATE FUNCTION execution_ops.notify_execution_result_ready_v1() RETURNS trigger
LANGUAGE plpgsql
AS $function$
BEGIN
    IF NEW.terminal_ts IS NOT NULL
       AND NEW.state IN ('SUCCEEDED', 'FAILED', 'CANCELLED', 'TIMED_OUT', 'UNKNOWN')
       AND (TG_OP = 'INSERT' OR OLD.terminal_ts IS NULL) THEN
        PERFORM pg_notify('execution_result_ready_v1', json_build_object(
            'version', 1,
            'originServiceId', NEW.origin_service_id,
            'originInstanceId', NEW.origin_instance_id,
            'subjectKind', NEW.subject_kind,
            'subjectId', NEW.subject_id,
            'subjectAttempt', NEW.attempt_number,
            'executionId', NEW.execution_id
        )::text);
    END IF;
    RETURN NEW;
END
$function$;

CREATE FUNCTION execution_ops.validate_runner_request_projection_v1() RETURNS trigger
LANGUAGE plpgsql
AS $function$
BEGIN
    IF NEW.origin_kind NOT IN ('workflow', 'agent') THEN
        RAISE EXCEPTION 'unsupported runner request origin %', NEW.origin_kind;
    END IF;
    IF NEW.normalized_requirements->>'policyDigest' IS DISTINCT FROM NEW.policy_digest THEN
        RAISE EXCEPTION 'runner request policy digest does not match normalized requirements';
    END IF;
    IF NEW.origin_reference_digest IS NULL OR NEW.origin_reference_digest = '' THEN
        RAISE EXCEPTION 'runner request has no immutable origin reference evidence';
    END IF;
    RETURN NEW;
END
$function$;

--
-- Name: execution_attempt_t; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE execution_ops.execution_attempt_t (
    host_id uuid NOT NULL,
    execution_id uuid NOT NULL,
    request_id uuid NOT NULL,
    origin_kind character varying(32) NOT NULL,
    origin_service_id character varying(255) NOT NULL,
    origin_instance_id character varying(255) NOT NULL,
    subject_kind character varying(32) NOT NULL,
    subject_id uuid NOT NULL,
    attempt_number integer NOT NULL,
    process_id uuid,
    task_id uuid,
    agent_session_id uuid,
    agent_turn_id uuid,
    agent_action_id uuid,
    lease_id uuid NOT NULL,
    fencing_token bigint NOT NULL,
    runner_session_id uuid NOT NULL,
    connection_generation bigint NOT NULL,
    backend_id character varying(126) NOT NULL,
    backend_operation_id character varying(255),
    state character varying(32) NOT NULL,
    lease_issued_ts timestamp with time zone NOT NULL,
    lease_started_ts timestamp with time zone,
    lease_renewed_ts timestamp with time zone,
    lease_deadline_ts timestamp with time zone NOT NULL,
    terminal_ts timestamp with time zone,
    normalized_result jsonb,
    normalized_error jsonb,
    retry_classification character varying(32),
    cleanup_state character varying(32) DEFAULT 'REQUIRED'::character varying NOT NULL,
    cleanup_evidence jsonb,
    accepted_by_origin_ts timestamp with time zone,
    workflow_reference_digest character varying(128),
    created_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    CONSTRAINT execution_attempt_t_attempt_number_check CHECK ((attempt_number > 0)),
    CONSTRAINT execution_attempt_t_check CHECK ((((state)::text = ANY (ARRAY[('SUCCEEDED'::character varying)::text, ('FAILED'::character varying)::text, ('CANCELLED'::character varying)::text, ('TIMED_OUT'::character varying)::text, ('UNKNOWN'::character varying)::text, ('CLEANED'::character varying)::text])) = (terminal_ts IS NOT NULL))),
    CONSTRAINT execution_attempt_t_cleanup_state_check CHECK (((cleanup_state)::text = ANY (ARRAY[('NOT_REQUIRED'::character varying)::text, ('REQUIRED'::character varying)::text, ('IN_PROGRESS'::character varying)::text, ('CONFIRMED'::character varying)::text, ('FAILED'::character varying)::text]))),
    CONSTRAINT execution_attempt_t_connection_generation_check CHECK ((connection_generation > 0)),
    CONSTRAINT execution_attempt_t_fencing_token_check CHECK ((fencing_token > 0)),
    CONSTRAINT execution_attempt_t_origin_kind_check CHECK (((origin_kind)::text = ANY (ARRAY[('workflow'::character varying)::text, ('agent'::character varying)::text]))),
    CONSTRAINT execution_attempt_t_retry_classification_check CHECK (((retry_classification IS NULL) OR ((retry_classification)::text = ANY (ARRAY[('safe'::character varying)::text, ('unsafe'::character varying)::text, ('inspect-required'::character varying)::text])))),
    CONSTRAINT execution_attempt_t_state_check CHECK (((state)::text = ANY (ARRAY[('CREATED'::character varying)::text, ('LEASED'::character varying)::text, ('STARTED'::character varying)::text, ('SUCCEEDED'::character varying)::text, ('FAILED'::character varying)::text, ('CANCELLED'::character varying)::text, ('TIMED_OUT'::character varying)::text, ('UNKNOWN'::character varying)::text, ('CLEANED'::character varying)::text]))),
    CONSTRAINT execution_attempt_t_subject_kind_check CHECK (((subject_kind)::text = ANY (ARRAY[('workflow-task'::character varying)::text, ('agent-turn'::character varying)::text, ('agent-action'::character varying)::text])))
);


--
-- Name: TABLE execution_attempt_t; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE execution_ops.execution_attempt_t IS 'Stores execution attempt records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN execution_attempt_t.host_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN execution_attempt_t.execution_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.execution_id IS 'Identifier for the related execution.';


--
-- Name: COLUMN execution_attempt_t.request_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.request_id IS 'Identifier for the related request.';


--
-- Name: COLUMN execution_attempt_t.origin_kind; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.origin_kind IS 'Origin Kind value for this execution attempt record.';


--
-- Name: COLUMN execution_attempt_t.origin_service_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.origin_service_id IS 'Identifier for the related origin service.';


--
-- Name: COLUMN execution_attempt_t.origin_instance_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.origin_instance_id IS 'Identifier for the related origin instance.';


--
-- Name: COLUMN execution_attempt_t.subject_kind; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.subject_kind IS 'Subject Kind value for this execution attempt record.';


--
-- Name: COLUMN execution_attempt_t.subject_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.subject_id IS 'Identifier for the related subject.';


--
-- Name: COLUMN execution_attempt_t.attempt_number; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.attempt_number IS 'Attempt Number value for this execution attempt record.';


--
-- Name: COLUMN execution_attempt_t.process_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.process_id IS 'Identifier for the related process.';


--
-- Name: COLUMN execution_attempt_t.task_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.task_id IS 'Identifier for the related task.';


--
-- Name: COLUMN execution_attempt_t.agent_session_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.agent_session_id IS 'Identifier for the related agent session.';


--
-- Name: COLUMN execution_attempt_t.agent_turn_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.agent_turn_id IS 'Identifier for the related agent turn.';


--
-- Name: COLUMN execution_attempt_t.agent_action_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.agent_action_id IS 'Identifier for the related agent action.';


--
-- Name: COLUMN execution_attempt_t.lease_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.lease_id IS 'Identifier for the related lease.';


--
-- Name: COLUMN execution_attempt_t.fencing_token; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.fencing_token IS 'Fencing Token value for this execution attempt record.';


--
-- Name: COLUMN execution_attempt_t.runner_session_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.runner_session_id IS 'Identifier for the related runner session.';


--
-- Name: COLUMN execution_attempt_t.connection_generation; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.connection_generation IS 'Connection Generation value for this execution attempt record.';


--
-- Name: COLUMN execution_attempt_t.backend_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.backend_id IS 'Identifier for the related backend.';


--
-- Name: COLUMN execution_attempt_t.backend_operation_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.backend_operation_id IS 'Identifier for the related backend operation.';


--
-- Name: COLUMN execution_attempt_t.state; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.state IS 'State value for this execution attempt record.';


--
-- Name: COLUMN execution_attempt_t.lease_issued_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.lease_issued_ts IS 'Timestamp for the lease issued event or state.';


--
-- Name: COLUMN execution_attempt_t.lease_started_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.lease_started_ts IS 'Timestamp for the lease started event or state.';


--
-- Name: COLUMN execution_attempt_t.lease_renewed_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.lease_renewed_ts IS 'Timestamp for the lease renewed event or state.';


--
-- Name: COLUMN execution_attempt_t.lease_deadline_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.lease_deadline_ts IS 'Timestamp for the lease deadline event or state.';


--
-- Name: COLUMN execution_attempt_t.terminal_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.terminal_ts IS 'Timestamp for the terminal event or state.';


--
-- Name: COLUMN execution_attempt_t.normalized_result; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.normalized_result IS 'Normalized Result value for this execution attempt record.';


--
-- Name: COLUMN execution_attempt_t.normalized_error; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.normalized_error IS 'Normalized Error value for this execution attempt record.';


--
-- Name: COLUMN execution_attempt_t.retry_classification; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.retry_classification IS 'Retry Classification value for this execution attempt record.';


--
-- Name: COLUMN execution_attempt_t.cleanup_state; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.cleanup_state IS 'Cleanup State value for this execution attempt record.';


--
-- Name: COLUMN execution_attempt_t.cleanup_evidence; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.cleanup_evidence IS 'Cleanup Evidence value for this execution attempt record.';


--
-- Name: COLUMN execution_attempt_t.accepted_by_origin_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.accepted_by_origin_ts IS 'Timestamp for the accepted by origin event or state.';


--
-- Name: COLUMN execution_attempt_t.workflow_reference_digest; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.workflow_reference_digest IS 'Digest binding Workflow process and task references admitted for this execution.';


--
-- Name: COLUMN execution_attempt_t.created_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.created_ts IS 'Timestamp for the created event or state.';


--
-- Name: COLUMN execution_attempt_t.updated_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_attempt_t.updated_ts IS 'Timestamp for the updated event or state.';


--
-- Name: execution_credential_grant_audit_t; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE execution_ops.execution_credential_grant_audit_t (
    host_id uuid NOT NULL,
    grant_id uuid NOT NULL,
    execution_id uuid NOT NULL,
    fencing_token bigint NOT NULL,
    policy_digest character varying(128) NOT NULL,
    operation character varying(126) NOT NULL,
    destination_digest character varying(128) NOT NULL,
    maximum_uses integer NOT NULL,
    use_count integer DEFAULT 0 NOT NULL,
    expires_ts timestamp with time zone NOT NULL,
    revoked_ts timestamp with time zone,
    revocation_reason character varying(255),
    created_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    CONSTRAINT execution_credential_grant_audit_t_check CHECK ((use_count <= maximum_uses)),
    CONSTRAINT execution_credential_grant_audit_t_maximum_uses_check CHECK ((maximum_uses > 0)),
    CONSTRAINT execution_credential_grant_audit_t_use_count_check CHECK ((use_count >= 0))
);


--
-- Name: TABLE execution_credential_grant_audit_t; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE execution_ops.execution_credential_grant_audit_t IS 'Stores execution credential grant audit records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN execution_credential_grant_audit_t.host_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_credential_grant_audit_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN execution_credential_grant_audit_t.grant_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_credential_grant_audit_t.grant_id IS 'Identifier for the related grant.';


--
-- Name: COLUMN execution_credential_grant_audit_t.execution_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_credential_grant_audit_t.execution_id IS 'Identifier for the related execution.';


--
-- Name: COLUMN execution_credential_grant_audit_t.fencing_token; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_credential_grant_audit_t.fencing_token IS 'Fencing Token value for this execution credential grant audit record.';


--
-- Name: COLUMN execution_credential_grant_audit_t.policy_digest; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_credential_grant_audit_t.policy_digest IS 'Integrity digest for policy.';


--
-- Name: COLUMN execution_credential_grant_audit_t.operation; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_credential_grant_audit_t.operation IS 'Operation value for this execution credential grant audit record.';


--
-- Name: COLUMN execution_credential_grant_audit_t.destination_digest; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_credential_grant_audit_t.destination_digest IS 'Integrity digest for destination.';


--
-- Name: COLUMN execution_credential_grant_audit_t.maximum_uses; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_credential_grant_audit_t.maximum_uses IS 'Maximum Uses value for this execution credential grant audit record.';


--
-- Name: COLUMN execution_credential_grant_audit_t.use_count; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_credential_grant_audit_t.use_count IS 'Count of use.';


--
-- Name: COLUMN execution_credential_grant_audit_t.expires_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_credential_grant_audit_t.expires_ts IS 'Timestamp for the expires event or state.';


--
-- Name: COLUMN execution_credential_grant_audit_t.revoked_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_credential_grant_audit_t.revoked_ts IS 'Timestamp for the revoked event or state.';


--
-- Name: COLUMN execution_credential_grant_audit_t.revocation_reason; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_credential_grant_audit_t.revocation_reason IS 'Revocation Reason value for this execution credential grant audit record.';


--
-- Name: COLUMN execution_credential_grant_audit_t.created_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_credential_grant_audit_t.created_ts IS 'Timestamp for the created event or state.';


--
-- Name: execution_fixed_action_t; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE execution_ops.execution_fixed_action_t (
    host_id uuid NOT NULL,
    fixed_action_id uuid NOT NULL,
    action_kind character varying(64) NOT NULL,
    execution_id uuid NOT NULL,
    approval_id uuid NOT NULL,
    repository_digest character varying(128) NOT NULL,
    base_commit character varying(64),
    repository_object_format character varying(16) DEFAULT 'sha1'::character varying NOT NULL,
    target_ref character varying(255) NOT NULL,
    artifact_digest character varying(128) NOT NULL,
    policy_digest character varying(128) NOT NULL,
    repository_reference text,
    patch_artifact_reference text,
    changed_paths jsonb DEFAULT '[]'::jsonb NOT NULL,
    action_spec jsonb DEFAULT '{}'::jsonb NOT NULL,
    provenance_digest character varying(128),
    idempotency_key character varying(255),
    provider_receipt jsonb,
    state character varying(32) NOT NULL,
    result_evidence jsonb,
    created_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    unknown_since_ts timestamp with time zone,
    next_reconcile_ts timestamp with time zone,
    reconciliation_attempt_count integer DEFAULT 0 NOT NULL,
    reconciliation_claim_token uuid,
    reconciliation_lease_expires_ts timestamp with time zone,
    approval_nonce_digest character varying(128),
    approval_expires_ts timestamp with time zone,
    approval_policy_digest character varying(128),
    approval_issuer character varying(255),
    CONSTRAINT execution_fixed_action_apply_patch_input_ck CHECK ((((action_kind)::text <> 'apply-patch'::text) OR ((repository_reference IS NOT NULL) AND (patch_artifact_reference IS NOT NULL) AND (jsonb_typeof(changed_paths) = 'array'::text)))),
    CONSTRAINT execution_fixed_action_base_commit_ck CHECK ((((action_kind)::text <> ALL (ARRAY[('apply-patch'::character varying)::text, ('create-branch'::character varying)::text, ('open-pr'::character varying)::text, ('push-commit'::character varying)::text])) OR ((((repository_object_format)::text = 'sha1'::text) AND ((base_commit)::text ~ '^[0-9A-Fa-f]{40}$'::text)) OR (((repository_object_format)::text = 'sha256'::text) AND ((base_commit)::text ~ '^[0-9A-Fa-f]{64}$'::text))))),
    CONSTRAINT execution_fixed_action_object_format_ck CHECK (((repository_object_format)::text = ANY (ARRAY[('sha1'::character varying)::text, ('sha256'::character varying)::text]))),
    CONSTRAINT execution_fixed_action_provider_input_ck CHECK ((((action_kind)::text <> ALL (ARRAY[('create-branch'::character varying)::text, ('open-pr'::character varying)::text, ('publish'::character varying)::text, ('sign'::character varying)::text])) OR ((jsonb_typeof(action_spec) = 'object'::text) AND (idempotency_key IS NOT NULL) AND ((length((idempotency_key)::text) >= 16) AND (length((idempotency_key)::text) <= 255))))),
    CONSTRAINT execution_fixed_action_reconciliation_ck CHECK (((reconciliation_attempt_count >= 0) AND (((reconciliation_claim_token IS NULL) AND (reconciliation_lease_expires_ts IS NULL)) OR (((state)::text = 'UNKNOWN'::text) AND (reconciliation_claim_token IS NOT NULL) AND (reconciliation_lease_expires_ts IS NOT NULL))) AND (((state)::text <> 'UNKNOWN'::text) OR (unknown_since_ts IS NOT NULL)))),
    CONSTRAINT execution_fixed_action_t_action_kind_check CHECK (((action_kind)::text = ANY (ARRAY[('apply-patch'::character varying)::text, ('create-branch'::character varying)::text, ('push-commit'::character varying)::text, ('open-pr'::character varying)::text, ('publish'::character varying)::text, ('sign'::character varying)::text]))),
    CONSTRAINT execution_fixed_action_t_state_check CHECK (((state)::text = ANY (ARRAY[('REQUESTED'::character varying)::text, ('VALIDATED'::character varying)::text, ('RUNNING'::character varying)::text, ('SUCCEEDED'::character varying)::text, ('FAILED'::character varying)::text, ('REJECTED'::character varying)::text, ('UNKNOWN'::character varying)::text])))
);


--
-- Name: TABLE execution_fixed_action_t; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE execution_ops.execution_fixed_action_t IS 'Stores execution fixed action records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN execution_fixed_action_t.host_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN execution_fixed_action_t.fixed_action_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.fixed_action_id IS 'Identifier for the related fixed action.';


--
-- Name: COLUMN execution_fixed_action_t.action_kind; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.action_kind IS 'Action Kind value for this execution fixed action record.';


--
-- Name: COLUMN execution_fixed_action_t.execution_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.execution_id IS 'Identifier for the related execution.';


--
-- Name: COLUMN execution_fixed_action_t.approval_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.approval_id IS 'Identifier for the related approval.';


--
-- Name: COLUMN execution_fixed_action_t.repository_digest; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.repository_digest IS 'Integrity digest for repository.';


--
-- Name: COLUMN execution_fixed_action_t.base_commit; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.base_commit IS 'Base Commit value for this execution fixed action record.';


--
-- Name: COLUMN execution_fixed_action_t.repository_object_format; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.repository_object_format IS 'Repository Object Format value for this execution fixed action record.';


--
-- Name: COLUMN execution_fixed_action_t.target_ref; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.target_ref IS 'Target Ref value for this execution fixed action record.';


--
-- Name: COLUMN execution_fixed_action_t.artifact_digest; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.artifact_digest IS 'Integrity digest for artifact.';


--
-- Name: COLUMN execution_fixed_action_t.policy_digest; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.policy_digest IS 'Integrity digest for policy.';


--
-- Name: COLUMN execution_fixed_action_t.repository_reference; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.repository_reference IS 'Repository Reference value for this execution fixed action record.';


--
-- Name: COLUMN execution_fixed_action_t.patch_artifact_reference; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.patch_artifact_reference IS 'Patch Artifact Reference value for this execution fixed action record.';


--
-- Name: COLUMN execution_fixed_action_t.changed_paths; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.changed_paths IS 'Changed Paths value for this execution fixed action record.';


--
-- Name: COLUMN execution_fixed_action_t.action_spec; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.action_spec IS 'Action Spec value for this execution fixed action record.';


--
-- Name: COLUMN execution_fixed_action_t.provenance_digest; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.provenance_digest IS 'Integrity digest for provenance.';


--
-- Name: COLUMN execution_fixed_action_t.idempotency_key; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.idempotency_key IS 'Idempotency Key value for this execution fixed action record.';


--
-- Name: COLUMN execution_fixed_action_t.provider_receipt; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.provider_receipt IS 'Provider Receipt value for this execution fixed action record.';


--
-- Name: COLUMN execution_fixed_action_t.state; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.state IS 'State value for this execution fixed action record.';


--
-- Name: COLUMN execution_fixed_action_t.result_evidence; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.result_evidence IS 'Result Evidence value for this execution fixed action record.';


--
-- Name: COLUMN execution_fixed_action_t.created_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.created_ts IS 'Timestamp for the created event or state.';


--
-- Name: COLUMN execution_fixed_action_t.updated_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.updated_ts IS 'Timestamp for the updated event or state.';


--
-- Name: COLUMN execution_fixed_action_t.unknown_since_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.unknown_since_ts IS 'Timestamp for the unknown since event or state.';


--
-- Name: COLUMN execution_fixed_action_t.next_reconcile_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.next_reconcile_ts IS 'Timestamp for the next reconcile event or state.';


--
-- Name: COLUMN execution_fixed_action_t.reconciliation_attempt_count; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.reconciliation_attempt_count IS 'Count of reconciliation attempt.';


--
-- Name: COLUMN execution_fixed_action_t.reconciliation_claim_token; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.reconciliation_claim_token IS 'Reconciliation Claim Token value for this execution fixed action record.';


--
-- Name: COLUMN execution_fixed_action_t.reconciliation_lease_expires_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.reconciliation_lease_expires_ts IS 'Timestamp for the reconciliation lease expires event or state.';


--
-- Name: COLUMN execution_fixed_action_t.approval_nonce_digest; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.approval_nonce_digest IS 'Pinned nonce digest from signed Workflow approval evidence.';


--
-- Name: COLUMN execution_fixed_action_t.approval_expires_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.approval_expires_ts IS 'Expiry from signed Workflow approval evidence.';


--
-- Name: COLUMN execution_fixed_action_t.approval_policy_digest; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.approval_policy_digest IS 'Policy digest from signed Workflow approval evidence.';


--
-- Name: COLUMN execution_fixed_action_t.approval_issuer; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_fixed_action_t.approval_issuer IS 'Authenticated issuer of the Workflow approval evidence.';


--
-- Name: execution_input_t; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE execution_ops.execution_input_t (
    host_id uuid NOT NULL,
    input_id uuid NOT NULL,
    request_id uuid NOT NULL,
    execution_id uuid,
    execution_session_id uuid,
    kind character varying(32) NOT NULL,
    artifact_uri text NOT NULL,
    content_digest character varying(128) NOT NULL,
    size_bytes bigint NOT NULL,
    media_type character varying(255) NOT NULL,
    signer_binding jsonb,
    provenance_binding jsonb,
    scanner_binding jsonb,
    revocation_binding jsonb,
    staging_root text NOT NULL,
    mount_target text NOT NULL,
    read_only boolean DEFAULT true NOT NULL,
    executable boolean DEFAULT false NOT NULL,
    staging_state character varying(32) DEFAULT 'PENDING'::character varying NOT NULL,
    verification_error character varying(255),
    created_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    trust_bundle_id character varying(126),
    trust_bundle_version integer,
    package_manifest_digest character varying(128),
    mount_options jsonb DEFAULT '["ro", "nodev", "nosuid", "noexec"]'::jsonb NOT NULL,
    CONSTRAINT execution_input_t_size_bytes_check CHECK ((size_bytes >= 0)),
    CONSTRAINT execution_input_t_staging_state_check CHECK (((staging_state)::text = ANY (ARRAY[('PENDING'::character varying)::text, ('STAGED'::character varying)::text, ('VERIFIED'::character varying)::text, ('REJECTED'::character varying)::text, ('REVOKED'::character varying)::text])))
);


--
-- Name: TABLE execution_input_t; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE execution_ops.execution_input_t IS 'Stores execution input records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN execution_input_t.host_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_input_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN execution_input_t.input_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_input_t.input_id IS 'Identifier for the related input.';


--
-- Name: COLUMN execution_input_t.request_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_input_t.request_id IS 'Identifier for the related request.';


--
-- Name: COLUMN execution_input_t.execution_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_input_t.execution_id IS 'Identifier for the related execution.';


--
-- Name: COLUMN execution_input_t.execution_session_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_input_t.execution_session_id IS 'Identifier for the related execution session.';


--
-- Name: COLUMN execution_input_t.kind; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_input_t.kind IS 'Kind value for this execution input record.';


--
-- Name: COLUMN execution_input_t.artifact_uri; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_input_t.artifact_uri IS 'Artifact Uri value for this execution input record.';


--
-- Name: COLUMN execution_input_t.content_digest; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_input_t.content_digest IS 'Integrity digest for content.';


--
-- Name: COLUMN execution_input_t.size_bytes; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_input_t.size_bytes IS 'Size Bytes value for this execution input record.';


--
-- Name: COLUMN execution_input_t.media_type; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_input_t.media_type IS 'Media Type value for this execution input record.';


--
-- Name: COLUMN execution_input_t.signer_binding; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_input_t.signer_binding IS 'Signer Binding value for this execution input record.';


--
-- Name: COLUMN execution_input_t.provenance_binding; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_input_t.provenance_binding IS 'Provenance Binding value for this execution input record.';


--
-- Name: COLUMN execution_input_t.scanner_binding; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_input_t.scanner_binding IS 'Scanner Binding value for this execution input record.';


--
-- Name: COLUMN execution_input_t.revocation_binding; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_input_t.revocation_binding IS 'Revocation Binding value for this execution input record.';


--
-- Name: COLUMN execution_input_t.staging_root; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_input_t.staging_root IS 'Staging Root value for this execution input record.';


--
-- Name: COLUMN execution_input_t.mount_target; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_input_t.mount_target IS 'Mount Target value for this execution input record.';


--
-- Name: COLUMN execution_input_t.read_only; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_input_t.read_only IS 'Read Only value for this execution input record.';


--
-- Name: COLUMN execution_input_t.executable; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_input_t.executable IS 'Executable value for this execution input record.';


--
-- Name: COLUMN execution_input_t.staging_state; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_input_t.staging_state IS 'Staging State value for this execution input record.';


--
-- Name: COLUMN execution_input_t.verification_error; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_input_t.verification_error IS 'Verification Error value for this execution input record.';


--
-- Name: COLUMN execution_input_t.created_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_input_t.created_ts IS 'Timestamp for the created event or state.';


--
-- Name: COLUMN execution_input_t.updated_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_input_t.updated_ts IS 'Timestamp for the updated event or state.';


--
-- Name: COLUMN execution_input_t.trust_bundle_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_input_t.trust_bundle_id IS 'Identifier for the related trust bundle.';


--
-- Name: COLUMN execution_input_t.trust_bundle_version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_input_t.trust_bundle_version IS 'Version value for trust bundle.';


--
-- Name: COLUMN execution_input_t.package_manifest_digest; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_input_t.package_manifest_digest IS 'Integrity digest for package manifest.';


--
-- Name: COLUMN execution_input_t.mount_options; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_input_t.mount_options IS 'Mount Options value for this execution input record.';


--
-- Name: execution_provenance_t; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE execution_ops.execution_provenance_t (
    host_id uuid NOT NULL,
    provenance_id uuid NOT NULL,
    execution_id uuid NOT NULL,
    statement jsonb NOT NULL,
    statement_digest character varying(128) NOT NULL,
    predicate_type character varying(255) NOT NULL,
    trusted_generator character varying(255) NOT NULL,
    signature_reference text,
    created_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL
);


--
-- Name: TABLE execution_provenance_t; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE execution_ops.execution_provenance_t IS 'Stores execution provenance records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN execution_provenance_t.host_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_provenance_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN execution_provenance_t.provenance_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_provenance_t.provenance_id IS 'Identifier for the related provenance.';


--
-- Name: COLUMN execution_provenance_t.execution_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_provenance_t.execution_id IS 'Identifier for the related execution.';


--
-- Name: COLUMN execution_provenance_t.statement; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_provenance_t.statement IS 'Statement value for this execution provenance record.';


--
-- Name: COLUMN execution_provenance_t.statement_digest; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_provenance_t.statement_digest IS 'Integrity digest for statement.';


--
-- Name: COLUMN execution_provenance_t.predicate_type; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_provenance_t.predicate_type IS 'Predicate Type value for this execution provenance record.';


--
-- Name: COLUMN execution_provenance_t.trusted_generator; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_provenance_t.trusted_generator IS 'Trusted Generator value for this execution provenance record.';


--
-- Name: COLUMN execution_provenance_t.signature_reference; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_provenance_t.signature_reference IS 'Signature Reference value for this execution provenance record.';


--
-- Name: COLUMN execution_provenance_t.created_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_provenance_t.created_ts IS 'Timestamp for the created event or state.';


--
-- Name: execution_runtime_audit_t; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE execution_ops.execution_runtime_audit_t (
    audit_id bigint NOT NULL,
    host_id uuid NOT NULL,
    origin_kind character varying(32) NOT NULL,
    origin_service_id character varying(255) NOT NULL,
    origin_instance_id character varying(255) NOT NULL,
    subject_kind character varying(32) NOT NULL,
    subject_id uuid NOT NULL,
    execution_id uuid,
    execution_session_id uuid,
    process_id uuid,
    task_id uuid,
    agent_session_id uuid,
    agent_turn_id uuid,
    agent_action_id uuid,
    actor character varying(255) NOT NULL,
    event_type character varying(126) NOT NULL,
    message_id uuid,
    lease_id uuid,
    fencing_token bigint,
    policy_digest character varying(64),
    redacted_payload jsonb DEFAULT '{}'::jsonb NOT NULL,
    event_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL
);


--
-- Name: TABLE execution_runtime_audit_t; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE execution_ops.execution_runtime_audit_t IS 'Stores execution runtime audit records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN execution_runtime_audit_t.audit_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_audit_t.audit_id IS 'Identifier for the related audit.';


--
-- Name: COLUMN execution_runtime_audit_t.host_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_audit_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN execution_runtime_audit_t.origin_kind; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_audit_t.origin_kind IS 'Origin Kind value for this execution runtime audit record.';


--
-- Name: COLUMN execution_runtime_audit_t.origin_service_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_audit_t.origin_service_id IS 'Identifier for the related origin service.';


--
-- Name: COLUMN execution_runtime_audit_t.origin_instance_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_audit_t.origin_instance_id IS 'Identifier for the related origin instance.';


--
-- Name: COLUMN execution_runtime_audit_t.subject_kind; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_audit_t.subject_kind IS 'Subject Kind value for this execution runtime audit record.';


--
-- Name: COLUMN execution_runtime_audit_t.subject_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_audit_t.subject_id IS 'Identifier for the related subject.';


--
-- Name: COLUMN execution_runtime_audit_t.execution_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_audit_t.execution_id IS 'Identifier for the related execution.';


--
-- Name: COLUMN execution_runtime_audit_t.execution_session_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_audit_t.execution_session_id IS 'Identifier for the related execution session.';


--
-- Name: COLUMN execution_runtime_audit_t.process_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_audit_t.process_id IS 'Identifier for the related process.';


--
-- Name: COLUMN execution_runtime_audit_t.task_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_audit_t.task_id IS 'Identifier for the related task.';


--
-- Name: COLUMN execution_runtime_audit_t.agent_session_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_audit_t.agent_session_id IS 'Identifier for the related agent session.';


--
-- Name: COLUMN execution_runtime_audit_t.agent_turn_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_audit_t.agent_turn_id IS 'Identifier for the related agent turn.';


--
-- Name: COLUMN execution_runtime_audit_t.agent_action_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_audit_t.agent_action_id IS 'Identifier for the related agent action.';


--
-- Name: COLUMN execution_runtime_audit_t.actor; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_audit_t.actor IS 'Actor value for this execution runtime audit record.';


--
-- Name: COLUMN execution_runtime_audit_t.event_type; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_audit_t.event_type IS 'Event Type value for this execution runtime audit record.';


--
-- Name: COLUMN execution_runtime_audit_t.message_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_audit_t.message_id IS 'Identifier for the related message.';


--
-- Name: COLUMN execution_runtime_audit_t.lease_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_audit_t.lease_id IS 'Identifier for the related lease.';


--
-- Name: COLUMN execution_runtime_audit_t.fencing_token; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_audit_t.fencing_token IS 'Fencing Token value for this execution runtime audit record.';


--
-- Name: COLUMN execution_runtime_audit_t.policy_digest; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_audit_t.policy_digest IS 'Integrity digest for policy.';


--
-- Name: COLUMN execution_runtime_audit_t.redacted_payload; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_audit_t.redacted_payload IS 'Redacted Payload value for this execution runtime audit record.';


--
-- Name: COLUMN execution_runtime_audit_t.event_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_audit_t.event_ts IS 'Timestamp for the event event or state.';


--
-- Name: execution_runtime_audit_t_audit_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE execution_ops.execution_runtime_audit_t ALTER COLUMN audit_id ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME execution_ops.execution_runtime_audit_t_audit_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: execution_runtime_tool_manifest_t; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE execution_ops.execution_runtime_tool_manifest_t (
    host_id uuid NOT NULL,
    manifest_id uuid NOT NULL,
    execution_id uuid NOT NULL,
    manifest jsonb NOT NULL,
    manifest_digest character varying(128) NOT NULL,
    signer_reference character varying(255) NOT NULL,
    verified_ts timestamp with time zone NOT NULL,
    created_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL
);


--
-- Name: TABLE execution_runtime_tool_manifest_t; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE execution_ops.execution_runtime_tool_manifest_t IS 'Stores execution runtime tool manifest records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN execution_runtime_tool_manifest_t.host_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_tool_manifest_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN execution_runtime_tool_manifest_t.manifest_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_tool_manifest_t.manifest_id IS 'Identifier for the related manifest.';


--
-- Name: COLUMN execution_runtime_tool_manifest_t.execution_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_tool_manifest_t.execution_id IS 'Identifier for the related execution.';


--
-- Name: COLUMN execution_runtime_tool_manifest_t.manifest; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_tool_manifest_t.manifest IS 'Manifest value for this execution runtime tool manifest record.';


--
-- Name: COLUMN execution_runtime_tool_manifest_t.manifest_digest; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_tool_manifest_t.manifest_digest IS 'Integrity digest for manifest.';


--
-- Name: COLUMN execution_runtime_tool_manifest_t.signer_reference; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_tool_manifest_t.signer_reference IS 'Signer Reference value for this execution runtime tool manifest record.';


--
-- Name: COLUMN execution_runtime_tool_manifest_t.verified_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_tool_manifest_t.verified_ts IS 'Timestamp for the verified event or state.';


--
-- Name: COLUMN execution_runtime_tool_manifest_t.created_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_runtime_tool_manifest_t.created_ts IS 'Timestamp for the created event or state.';


--
-- Name: execution_session_cleanup_request_t; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE execution_ops.execution_session_cleanup_request_t (
    host_id uuid NOT NULL,
    cleanup_request_id uuid NOT NULL,
    execution_session_id uuid NOT NULL,
    origin_kind character varying(32) NOT NULL,
    origin_service_id character varying(255) NOT NULL,
    origin_instance_id character varying(255) NOT NULL,
    origin_session_id uuid,
    subject_kind character varying(32) NOT NULL,
    subject_id uuid NOT NULL,
    idempotency_key character varying(255) NOT NULL,
    reason character varying(64) NOT NULL,
    requested_by character varying(255) NOT NULL,
    requested_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    cleanup_deadline_ts timestamp with time zone NOT NULL,
    state character varying(32) NOT NULL,
    runner_ack_ts timestamp with time zone,
    cleanup_evidence jsonb,
    updated_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    CONSTRAINT execution_session_cleanup_request_t_state_check CHECK (((state)::text = ANY (ARRAY[('PENDING'::character varying)::text, ('FENCED'::character varying)::text, ('DELIVERED'::character varying)::text, ('CLEANED'::character varying)::text, ('FAILED'::character varying)::text, ('EXPIRED'::character varying)::text])))
);


--
-- Name: TABLE execution_session_cleanup_request_t; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE execution_ops.execution_session_cleanup_request_t IS 'Stores execution session cleanup request records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN execution_session_cleanup_request_t.host_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_cleanup_request_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN execution_session_cleanup_request_t.cleanup_request_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_cleanup_request_t.cleanup_request_id IS 'Identifier for the related cleanup request.';


--
-- Name: COLUMN execution_session_cleanup_request_t.execution_session_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_cleanup_request_t.execution_session_id IS 'Identifier for the related execution session.';


--
-- Name: COLUMN execution_session_cleanup_request_t.origin_kind; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_cleanup_request_t.origin_kind IS 'Origin Kind value for this execution session cleanup request record.';


--
-- Name: COLUMN execution_session_cleanup_request_t.origin_service_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_cleanup_request_t.origin_service_id IS 'Identifier for the related origin service.';


--
-- Name: COLUMN execution_session_cleanup_request_t.origin_instance_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_cleanup_request_t.origin_instance_id IS 'Identifier for the related origin instance.';


--
-- Name: COLUMN execution_session_cleanup_request_t.origin_session_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_cleanup_request_t.origin_session_id IS 'Identifier for the related origin session.';


--
-- Name: COLUMN execution_session_cleanup_request_t.subject_kind; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_cleanup_request_t.subject_kind IS 'Subject Kind value for this execution session cleanup request record.';


--
-- Name: COLUMN execution_session_cleanup_request_t.subject_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_cleanup_request_t.subject_id IS 'Identifier for the related subject.';


--
-- Name: COLUMN execution_session_cleanup_request_t.idempotency_key; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_cleanup_request_t.idempotency_key IS 'Idempotency Key value for this execution session cleanup request record.';


--
-- Name: COLUMN execution_session_cleanup_request_t.reason; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_cleanup_request_t.reason IS 'Reason value for this execution session cleanup request record.';


--
-- Name: COLUMN execution_session_cleanup_request_t.requested_by; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_cleanup_request_t.requested_by IS 'Requested By value for this execution session cleanup request record.';


--
-- Name: COLUMN execution_session_cleanup_request_t.requested_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_cleanup_request_t.requested_ts IS 'Timestamp for the requested event or state.';


--
-- Name: COLUMN execution_session_cleanup_request_t.cleanup_deadline_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_cleanup_request_t.cleanup_deadline_ts IS 'Timestamp for the cleanup deadline event or state.';


--
-- Name: COLUMN execution_session_cleanup_request_t.state; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_cleanup_request_t.state IS 'State value for this execution session cleanup request record.';


--
-- Name: COLUMN execution_session_cleanup_request_t.runner_ack_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_cleanup_request_t.runner_ack_ts IS 'Timestamp for the runner ack event or state.';


--
-- Name: COLUMN execution_session_cleanup_request_t.cleanup_evidence; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_cleanup_request_t.cleanup_evidence IS 'Cleanup Evidence value for this execution session cleanup request record.';


--
-- Name: COLUMN execution_session_cleanup_request_t.updated_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_cleanup_request_t.updated_ts IS 'Timestamp for the updated event or state.';


--
-- Name: execution_session_t; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE execution_ops.execution_session_t (
    host_id uuid NOT NULL,
    execution_session_id uuid NOT NULL,
    origin_kind character varying(32) NOT NULL,
    origin_service_id character varying(255) NOT NULL,
    origin_instance_id character varying(255) NOT NULL,
    subject_kind character varying(32) NOT NULL,
    subject_id uuid NOT NULL,
    origin_session_id uuid,
    policy_digest character varying(64) NOT NULL,
    compatibility_digest character varying(128) NOT NULL,
    runner_session_id uuid NOT NULL,
    backend_id character varying(126) NOT NULL,
    backend_session_handle character varying(255),
    checkpoint_handle character varying(255),
    idle_expires_ts timestamp with time zone,
    maximum_expires_ts timestamp with time zone NOT NULL,
    effective_expires_ts timestamp with time zone NOT NULL,
    state character varying(32) NOT NULL,
    session_version bigint NOT NULL,
    session_fence bigint NOT NULL,
    hold_id uuid,
    hold_reason character varying(126),
    hold_until_ts timestamp with time zone,
    hold_policy_digest character varying(64),
    retained_resource_evidence jsonb,
    cleanup_status character varying(32) DEFAULT 'NOT_REQUESTED'::character varying NOT NULL,
    cleanup_evidence jsonb,
    created_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    CONSTRAINT execution_session_t_check CHECK (((((state)::text = 'IDLE_APPROVAL_HOLD'::text) AND (hold_id IS NOT NULL) AND (hold_until_ts IS NOT NULL)) OR ((state)::text <> 'IDLE_APPROVAL_HOLD'::text))),
    CONSTRAINT execution_session_t_cleanup_status_check CHECK (((cleanup_status)::text = ANY (ARRAY[('NOT_REQUESTED'::character varying)::text, ('PENDING'::character varying)::text, ('CLEANING'::character varying)::text, ('CLEANED'::character varying)::text, ('FAILED'::character varying)::text]))),
    CONSTRAINT execution_session_t_session_fence_check CHECK ((session_fence > 0)),
    CONSTRAINT execution_session_t_session_version_check CHECK ((session_version > 0)),
    CONSTRAINT execution_session_t_state_check CHECK (((state)::text = ANY (ARRAY[('READY'::character varying)::text, ('ACTIVE_ACTION'::character varying)::text, ('IDLE'::character varying)::text, ('IDLE_APPROVAL_HOLD'::character varying)::text, ('CLEANUP_REQUESTED'::character varying)::text, ('CLEANING'::character varying)::text, ('CLEANED'::character varying)::text, ('FAILED'::character varying)::text])))
);


--
-- Name: TABLE execution_session_t; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE execution_ops.execution_session_t IS 'Stores execution session records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN execution_session_t.host_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN execution_session_t.execution_session_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.execution_session_id IS 'Identifier for the related execution session.';


--
-- Name: COLUMN execution_session_t.origin_kind; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.origin_kind IS 'Origin Kind value for this execution session record.';


--
-- Name: COLUMN execution_session_t.origin_service_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.origin_service_id IS 'Identifier for the related origin service.';


--
-- Name: COLUMN execution_session_t.origin_instance_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.origin_instance_id IS 'Identifier for the related origin instance.';


--
-- Name: COLUMN execution_session_t.subject_kind; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.subject_kind IS 'Subject Kind value for this execution session record.';


--
-- Name: COLUMN execution_session_t.subject_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.subject_id IS 'Identifier for the related subject.';


--
-- Name: COLUMN execution_session_t.origin_session_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.origin_session_id IS 'Identifier for the related origin session.';


--
-- Name: COLUMN execution_session_t.policy_digest; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.policy_digest IS 'Integrity digest for policy.';


--
-- Name: COLUMN execution_session_t.compatibility_digest; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.compatibility_digest IS 'Integrity digest for compatibility.';


--
-- Name: COLUMN execution_session_t.runner_session_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.runner_session_id IS 'Identifier for the related runner session.';


--
-- Name: COLUMN execution_session_t.backend_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.backend_id IS 'Identifier for the related backend.';


--
-- Name: COLUMN execution_session_t.backend_session_handle; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.backend_session_handle IS 'Backend Session Handle value for this execution session record.';


--
-- Name: COLUMN execution_session_t.checkpoint_handle; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.checkpoint_handle IS 'Checkpoint Handle value for this execution session record.';


--
-- Name: COLUMN execution_session_t.idle_expires_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.idle_expires_ts IS 'Timestamp for the idle expires event or state.';


--
-- Name: COLUMN execution_session_t.maximum_expires_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.maximum_expires_ts IS 'Timestamp for the maximum expires event or state.';


--
-- Name: COLUMN execution_session_t.effective_expires_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.effective_expires_ts IS 'Timestamp for the effective expires event or state.';


--
-- Name: COLUMN execution_session_t.state; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.state IS 'State value for this execution session record.';


--
-- Name: COLUMN execution_session_t.session_version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.session_version IS 'Version value for session.';


--
-- Name: COLUMN execution_session_t.session_fence; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.session_fence IS 'Session Fence value for this execution session record.';


--
-- Name: COLUMN execution_session_t.hold_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.hold_id IS 'Identifier for the related hold.';


--
-- Name: COLUMN execution_session_t.hold_reason; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.hold_reason IS 'Hold Reason value for this execution session record.';


--
-- Name: COLUMN execution_session_t.hold_until_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.hold_until_ts IS 'Timestamp for the hold until event or state.';


--
-- Name: COLUMN execution_session_t.hold_policy_digest; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.hold_policy_digest IS 'Integrity digest for hold policy.';


--
-- Name: COLUMN execution_session_t.retained_resource_evidence; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.retained_resource_evidence IS 'Retained Resource Evidence value for this execution session record.';


--
-- Name: COLUMN execution_session_t.cleanup_status; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.cleanup_status IS 'Cleanup Status value for this execution session record.';


--
-- Name: COLUMN execution_session_t.cleanup_evidence; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.cleanup_evidence IS 'Cleanup Evidence value for this execution session record.';


--
-- Name: COLUMN execution_session_t.created_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.created_ts IS 'Timestamp for the created event or state.';


--
-- Name: COLUMN execution_session_t.updated_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.execution_session_t.updated_ts IS 'Timestamp for the updated event or state.';


--
-- Name: runner_backend_t; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE execution_ops.runner_backend_t (
    host_id uuid NOT NULL,
    session_id uuid NOT NULL,
    backend_id character varying(126) NOT NULL,
    backend_version character varying(64) NOT NULL,
    boundary_class character varying(32) NOT NULL,
    host_exposure_class character varying(32) NOT NULL,
    supported_actions jsonb DEFAULT '[]'::jsonb NOT NULL,
    supported_features jsonb DEFAULT '[]'::jsonb NOT NULL,
    capability_limits jsonb DEFAULT '{}'::jsonb NOT NULL,
    compatibility_digest character varying(128) NOT NULL,
    health character varying(32) NOT NULL,
    available_slots integer DEFAULT 0 NOT NULL,
    observed_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    CONSTRAINT runner_backend_t_available_slots_check CHECK ((available_slots >= 0))
);


--
-- Name: TABLE runner_backend_t; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE execution_ops.runner_backend_t IS 'Stores runner backend records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN runner_backend_t.host_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_backend_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN runner_backend_t.session_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_backend_t.session_id IS 'Identifier for the related session.';


--
-- Name: COLUMN runner_backend_t.backend_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_backend_t.backend_id IS 'Identifier for the related backend.';


--
-- Name: COLUMN runner_backend_t.backend_version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_backend_t.backend_version IS 'Version value for backend.';


--
-- Name: COLUMN runner_backend_t.boundary_class; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_backend_t.boundary_class IS 'Boundary Class value for this runner backend record.';


--
-- Name: COLUMN runner_backend_t.host_exposure_class; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_backend_t.host_exposure_class IS 'Host Exposure Class value for this runner backend record.';


--
-- Name: COLUMN runner_backend_t.supported_actions; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_backend_t.supported_actions IS 'Supported Actions value for this runner backend record.';


--
-- Name: COLUMN runner_backend_t.supported_features; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_backend_t.supported_features IS 'Supported Features value for this runner backend record.';


--
-- Name: COLUMN runner_backend_t.capability_limits; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_backend_t.capability_limits IS 'Capability Limits value for this runner backend record.';


--
-- Name: COLUMN runner_backend_t.compatibility_digest; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_backend_t.compatibility_digest IS 'Integrity digest for compatibility.';


--
-- Name: COLUMN runner_backend_t.health; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_backend_t.health IS 'Health value for this runner backend record.';


--
-- Name: COLUMN runner_backend_t.available_slots; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_backend_t.available_slots IS 'Available Slots value for this runner backend record.';


--
-- Name: COLUMN runner_backend_t.observed_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_backend_t.observed_ts IS 'Timestamp for the observed event or state.';


--
-- Name: runner_scheduling_request_t; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE execution_ops.runner_scheduling_request_t (
    host_id uuid NOT NULL,
    request_id uuid NOT NULL,
    request_digest character varying(71) NOT NULL,
    idempotency_key character varying(255) NOT NULL,
    origin_kind character varying(32) NOT NULL,
    origin_service_id character varying(255) NOT NULL,
    origin_instance_id character varying(255) NOT NULL,
    subject_kind character varying(32) NOT NULL,
    subject_id uuid NOT NULL,
    process_id uuid,
    task_id uuid,
    agent_session_id uuid,
    agent_turn_id uuid,
    agent_action_id uuid,
    policy_snapshot_id uuid NOT NULL,
    policy_digest character varying(71) NOT NULL,
    normalized_requirements jsonb NOT NULL,
    execution_spec jsonb DEFAULT '{}'::jsonb NOT NULL,
    fairness_key character varying(255) NOT NULL,
    priority integer DEFAULT 0 NOT NULL,
    queue_sequence bigint NOT NULL,
    not_before_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    state character varying(32) NOT NULL,
    selected_runner_session_id uuid,
    selected_backend_id character varying(126),
    reservation_token_hash character varying(128),
    reservation_expires_ts timestamp with time zone,
    retry_count integer DEFAULT 0 NOT NULL,
    next_retry_ts timestamp with time zone,
    diagnostic_reason character varying(255),
    approval_id uuid,
    pinned_runner_id character varying(126),
    pinned_backend_id character varying(126),
    edge_binding_id uuid,
    workflow_reference_digest character varying(128),
    approval_evidence_digest character varying(128),
    edge_binding_compatibility_digest character varying(128),
    edge_binding_revocation_epoch bigint,
    origin_reference_digest character varying(128) NOT NULL,
    resolved_policy jsonb NOT NULL,
    definition_digest character varying(128) NOT NULL,
    created_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    CONSTRAINT runner_scheduling_request_t_check CHECK (((((subject_kind)::text = 'workflow-task'::text) AND (process_id IS NOT NULL) AND (task_id IS NOT NULL) AND (agent_session_id IS NULL) AND (agent_turn_id IS NULL) AND (agent_action_id IS NULL)) OR (((subject_kind)::text = 'agent-turn'::text) AND (process_id IS NULL) AND (task_id IS NULL) AND (agent_session_id IS NOT NULL) AND (agent_turn_id IS NOT NULL) AND (agent_action_id IS NULL)) OR (((subject_kind)::text = 'agent-action'::text) AND (process_id IS NULL) AND (task_id IS NULL) AND (agent_session_id IS NOT NULL) AND (agent_turn_id IS NOT NULL) AND (agent_action_id IS NOT NULL)))),
    CONSTRAINT runner_scheduling_request_t_origin_kind_check CHECK (((origin_kind)::text = ANY (ARRAY[('workflow'::character varying)::text, ('agent'::character varying)::text]))),
    CONSTRAINT runner_scheduling_request_t_request_digest_check CHECK (((request_digest)::text ~ '^sha256:[0-9a-f]{64}$'::text)),
    CONSTRAINT runner_scheduling_request_t_origin_reference_check CHECK ((length(origin_reference_digest) > 0)),
    CONSTRAINT runner_scheduling_request_t_projection_check CHECK ((jsonb_typeof(resolved_policy) = 'object'::text AND length(definition_digest) > 0)),
    CONSTRAINT runner_scheduling_request_t_approval_evidence_check CHECK ((approval_id IS NULL OR approval_evidence_digest IS NOT NULL)),
    CONSTRAINT runner_scheduling_request_t_edge_evidence_check CHECK ((edge_binding_id IS NULL OR (pinned_runner_id IS NOT NULL AND pinned_backend_id IS NOT NULL AND edge_binding_compatibility_digest IS NOT NULL AND edge_binding_revocation_epoch IS NOT NULL AND edge_binding_revocation_epoch >= 0))),
    CONSTRAINT runner_scheduling_request_t_retry_count_check CHECK ((retry_count >= 0)),
    CONSTRAINT runner_scheduling_request_t_state_check CHECK (((state)::text = ANY (ARRAY[('PENDING_CAPACITY'::character varying)::text, ('RESERVED'::character varying)::text, ('ATTEMPT_CREATED'::character varying)::text, ('LEASED'::character varying)::text, ('SATISFIED'::character varying)::text, ('CANCELLED'::character varying)::text, ('EXPIRED'::character varying)::text]))),
    CONSTRAINT runner_scheduling_request_t_subject_kind_check CHECK (((subject_kind)::text = ANY (ARRAY[('workflow-task'::character varying)::text, ('agent-turn'::character varying)::text, ('agent-action'::character varying)::text])))
);


--
-- Name: TABLE runner_scheduling_request_t; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE execution_ops.runner_scheduling_request_t IS 'Stores runner scheduling request records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN runner_scheduling_request_t.host_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN runner_scheduling_request_t.request_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.request_id IS 'Identifier for the related request.';


--
-- Name: COLUMN runner_scheduling_request_t.idempotency_key; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.idempotency_key IS 'Idempotency Key value for this runner scheduling request record.';


--
-- Name: COLUMN runner_scheduling_request_t.origin_kind; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.origin_kind IS 'Origin Kind value for this runner scheduling request record.';


--
-- Name: COLUMN runner_scheduling_request_t.origin_service_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.origin_service_id IS 'Identifier for the related origin service.';


--
-- Name: COLUMN runner_scheduling_request_t.origin_instance_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.origin_instance_id IS 'Identifier for the related origin instance.';


--
-- Name: COLUMN runner_scheduling_request_t.subject_kind; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.subject_kind IS 'Subject Kind value for this runner scheduling request record.';


--
-- Name: COLUMN runner_scheduling_request_t.subject_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.subject_id IS 'Identifier for the related subject.';


--
-- Name: COLUMN runner_scheduling_request_t.process_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.process_id IS 'Identifier for the related process.';


--
-- Name: COLUMN runner_scheduling_request_t.task_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.task_id IS 'Identifier for the related task.';


--
-- Name: COLUMN runner_scheduling_request_t.agent_session_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.agent_session_id IS 'Identifier for the related agent session.';


--
-- Name: COLUMN runner_scheduling_request_t.agent_turn_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.agent_turn_id IS 'Identifier for the related agent turn.';


--
-- Name: COLUMN runner_scheduling_request_t.agent_action_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.agent_action_id IS 'Identifier for the related agent action.';


--
-- Name: COLUMN runner_scheduling_request_t.policy_snapshot_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.policy_snapshot_id IS 'Identifier for the related policy snapshot.';


--
-- Name: COLUMN runner_scheduling_request_t.policy_digest; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.policy_digest IS 'Integrity digest for policy.';


--
-- Name: COLUMN runner_scheduling_request_t.normalized_requirements; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.normalized_requirements IS 'Normalized Requirements value for this runner scheduling request record.';


--
-- Name: COLUMN runner_scheduling_request_t.execution_spec; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.execution_spec IS 'Execution Spec value for this runner scheduling request record.';


--
-- Name: COLUMN runner_scheduling_request_t.fairness_key; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.fairness_key IS 'Fairness Key value for this runner scheduling request record.';


--
-- Name: COLUMN runner_scheduling_request_t.priority; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.priority IS 'Priority value for this runner scheduling request record.';


--
-- Name: COLUMN runner_scheduling_request_t.queue_sequence; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.queue_sequence IS 'Queue Sequence value for this runner scheduling request record.';


--
-- Name: COLUMN runner_scheduling_request_t.not_before_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.not_before_ts IS 'Timestamp for the not before event or state.';


--
-- Name: COLUMN runner_scheduling_request_t.state; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.state IS 'State value for this runner scheduling request record.';


--
-- Name: COLUMN runner_scheduling_request_t.selected_runner_session_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.selected_runner_session_id IS 'Identifier for the related selected runner session.';


--
-- Name: COLUMN runner_scheduling_request_t.selected_backend_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.selected_backend_id IS 'Identifier for the related selected backend.';


--
-- Name: COLUMN runner_scheduling_request_t.reservation_token_hash; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.reservation_token_hash IS 'Integrity digest for reservation token.';


--
-- Name: COLUMN runner_scheduling_request_t.reservation_expires_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.reservation_expires_ts IS 'Timestamp for the reservation expires event or state.';


--
-- Name: COLUMN runner_scheduling_request_t.retry_count; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.retry_count IS 'Count of retry.';


--
-- Name: COLUMN runner_scheduling_request_t.next_retry_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.next_retry_ts IS 'Timestamp for the next retry event or state.';


--
-- Name: COLUMN runner_scheduling_request_t.diagnostic_reason; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.diagnostic_reason IS 'Diagnostic Reason value for this runner scheduling request record.';


--
-- Name: COLUMN runner_scheduling_request_t.approval_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.approval_id IS 'Identifier for the related approval.';


--
-- Name: COLUMN runner_scheduling_request_t.pinned_runner_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.pinned_runner_id IS 'Identifier for the related pinned runner.';


--
-- Name: COLUMN runner_scheduling_request_t.pinned_backend_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.pinned_backend_id IS 'Identifier for the related pinned backend.';


--
-- Name: COLUMN runner_scheduling_request_t.edge_binding_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.edge_binding_id IS 'Identifier for the related edge binding.';


--
-- Name: COLUMN runner_scheduling_request_t.workflow_reference_digest; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.workflow_reference_digest IS 'Digest binding admitted Workflow process and task references.';


--
-- Name: COLUMN runner_scheduling_request_t.approval_evidence_digest; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.approval_evidence_digest IS 'Digest of signed Workflow approval evidence accepted before reservation.';


--
-- Name: COLUMN runner_scheduling_request_t.edge_binding_compatibility_digest; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.edge_binding_compatibility_digest IS 'Pinned compatibility digest from the runner-binding projection.';


--
-- Name: COLUMN runner_scheduling_request_t.edge_binding_revocation_epoch; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.edge_binding_revocation_epoch IS 'Pinned revocation epoch from the runner-binding projection.';


--
-- Name: COLUMN runner_scheduling_request_t.created_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.created_ts IS 'Timestamp for the created event or state.';


--
-- Name: COLUMN runner_scheduling_request_t.updated_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_scheduling_request_t.updated_ts IS 'Timestamp for the updated event or state.';


--
-- Name: runner_scheduling_request_t_queue_sequence_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE execution_ops.runner_scheduling_request_t ALTER COLUMN queue_sequence ADD GENERATED BY DEFAULT AS IDENTITY (
    SEQUENCE NAME execution_ops.runner_scheduling_request_t_queue_sequence_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: runner_session_t; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE execution_ops.runner_session_t (
    host_id uuid NOT NULL,
    session_id uuid NOT NULL,
    runner_id character varying(126) NOT NULL,
    authenticated_subject character varying(255) NOT NULL,
    enrollment_id character varying(126) NOT NULL,
    runner_version character varying(64) NOT NULL,
    protocol_version character varying(32) NOT NULL,
    connection_generation bigint NOT NULL,
    status character varying(32) NOT NULL,
    drain_state character varying(32) DEFAULT 'ACCEPTING'::character varying NOT NULL,
    binary_digest character varying(128) NOT NULL,
    effective_config_digest character varying(128) NOT NULL,
    command_allowlist_digest character varying(128) NOT NULL,
    capability_document jsonb NOT NULL,
    compatibility_digest character varying(128) NOT NULL,
    maximum_concurrency integer NOT NULL,
    reported_available_capacity integer DEFAULT 0 NOT NULL,
    watchdog_healthy boolean NOT NULL,
    journal_healthy boolean NOT NULL,
    cleanup_backlog integer DEFAULT 0 NOT NULL,
    registered_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    heartbeat_ts timestamp with time zone,
    disconnected_ts timestamp with time zone,
    environment character varying(64),
    scope_binding_digest character varying(128),
    CONSTRAINT runner_session_t_cleanup_backlog_check CHECK ((cleanup_backlog >= 0)),
    CONSTRAINT runner_session_t_connection_generation_check CHECK ((connection_generation > 0)),
    CONSTRAINT runner_session_t_maximum_concurrency_check CHECK ((maximum_concurrency > 0)),
    CONSTRAINT runner_session_t_reported_available_capacity_check CHECK ((reported_available_capacity >= 0))
);


--
-- Name: TABLE runner_session_t; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE execution_ops.runner_session_t IS 'Stores runner session records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN runner_session_t.host_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_session_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN runner_session_t.session_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_session_t.session_id IS 'Identifier for the related session.';


--
-- Name: COLUMN runner_session_t.runner_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_session_t.runner_id IS 'Identifier for the related runner.';


--
-- Name: COLUMN runner_session_t.authenticated_subject; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_session_t.authenticated_subject IS 'Authenticated Subject value for this runner session record.';


--
-- Name: COLUMN runner_session_t.enrollment_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_session_t.enrollment_id IS 'Identifier for the related enrollment.';


--
-- Name: COLUMN runner_session_t.runner_version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_session_t.runner_version IS 'Version value for runner.';


--
-- Name: COLUMN runner_session_t.protocol_version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_session_t.protocol_version IS 'Version value for protocol.';


--
-- Name: COLUMN runner_session_t.connection_generation; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_session_t.connection_generation IS 'Connection Generation value for this runner session record.';


--
-- Name: COLUMN runner_session_t.status; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_session_t.status IS 'Status value for this runner session record.';


--
-- Name: COLUMN runner_session_t.drain_state; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_session_t.drain_state IS 'Drain State value for this runner session record.';


--
-- Name: COLUMN runner_session_t.binary_digest; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_session_t.binary_digest IS 'Integrity digest for binary.';


--
-- Name: COLUMN runner_session_t.effective_config_digest; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_session_t.effective_config_digest IS 'Integrity digest for effective config.';


--
-- Name: COLUMN runner_session_t.command_allowlist_digest; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_session_t.command_allowlist_digest IS 'Integrity digest for command allowlist.';


--
-- Name: COLUMN runner_session_t.capability_document; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_session_t.capability_document IS 'Capability Document value for this runner session record.';


--
-- Name: COLUMN runner_session_t.compatibility_digest; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_session_t.compatibility_digest IS 'Integrity digest for compatibility.';


--
-- Name: COLUMN runner_session_t.maximum_concurrency; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_session_t.maximum_concurrency IS 'Maximum Concurrency value for this runner session record.';


--
-- Name: COLUMN runner_session_t.reported_available_capacity; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_session_t.reported_available_capacity IS 'Reported Available Capacity value for this runner session record.';


--
-- Name: COLUMN runner_session_t.watchdog_healthy; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_session_t.watchdog_healthy IS 'Watchdog Healthy value for this runner session record.';


--
-- Name: COLUMN runner_session_t.journal_healthy; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_session_t.journal_healthy IS 'Journal Healthy value for this runner session record.';


--
-- Name: COLUMN runner_session_t.cleanup_backlog; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_session_t.cleanup_backlog IS 'Cleanup Backlog value for this runner session record.';


--
-- Name: COLUMN runner_session_t.registered_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_session_t.registered_ts IS 'Timestamp for the registered event or state.';


--
-- Name: COLUMN runner_session_t.heartbeat_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_session_t.heartbeat_ts IS 'Timestamp for the heartbeat event or state.';


--
-- Name: COLUMN runner_session_t.disconnected_ts; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_session_t.disconnected_ts IS 'Timestamp for the disconnected event or state.';


--
-- Name: COLUMN runner_session_t.environment; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_session_t.environment IS 'Environment accepted from the runner operational scope.';


--
-- Name: COLUMN runner_session_t.scope_binding_digest; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN execution_ops.runner_session_t.scope_binding_digest IS 'Digest of the Host and environment binding accepted at registration.';


--
-- Name: execution_attempt_t execution_attempt_t_host_id_origin_service_id_origin_insta_key1; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.execution_attempt_t
    ADD CONSTRAINT execution_attempt_t_host_id_origin_service_id_origin_insta_key1 UNIQUE (host_id, origin_service_id, origin_instance_id, subject_kind, subject_id, fencing_token);


--
-- Name: execution_attempt_t execution_attempt_t_host_id_origin_service_id_origin_instan_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.execution_attempt_t
    ADD CONSTRAINT execution_attempt_t_host_id_origin_service_id_origin_instan_key UNIQUE (host_id, origin_service_id, origin_instance_id, subject_kind, subject_id, attempt_number);


--
-- Name: execution_attempt_t execution_attempt_t_lease_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.execution_attempt_t
    ADD CONSTRAINT execution_attempt_t_lease_id_key UNIQUE (lease_id);


--
-- Name: execution_attempt_t execution_attempt_t_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.execution_attempt_t
    ADD CONSTRAINT execution_attempt_t_pkey PRIMARY KEY (host_id, execution_id);


--
-- Name: execution_credential_grant_audit_t execution_credential_grant_audit_t_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.execution_credential_grant_audit_t
    ADD CONSTRAINT execution_credential_grant_audit_t_pkey PRIMARY KEY (host_id, grant_id);


--
-- Name: execution_fixed_action_t execution_fixed_action_t_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.execution_fixed_action_t
    ADD CONSTRAINT execution_fixed_action_t_pkey PRIMARY KEY (host_id, fixed_action_id);


--
-- Name: execution_input_t execution_input_t_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.execution_input_t
    ADD CONSTRAINT execution_input_t_pkey PRIMARY KEY (host_id, input_id);


--
-- Name: execution_provenance_t execution_provenance_t_host_id_execution_id_statement_diges_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.execution_provenance_t
    ADD CONSTRAINT execution_provenance_t_host_id_execution_id_statement_diges_key UNIQUE (host_id, execution_id, statement_digest);


--
-- Name: execution_provenance_t execution_provenance_t_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.execution_provenance_t
    ADD CONSTRAINT execution_provenance_t_pkey PRIMARY KEY (host_id, provenance_id);


--
-- Name: execution_runtime_audit_t execution_runtime_audit_t_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.execution_runtime_audit_t
    ADD CONSTRAINT execution_runtime_audit_t_pkey PRIMARY KEY (audit_id);


--
-- Name: execution_runtime_tool_manifest_t execution_runtime_tool_manife_host_id_execution_id_manifest_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.execution_runtime_tool_manifest_t
    ADD CONSTRAINT execution_runtime_tool_manife_host_id_execution_id_manifest_key UNIQUE (host_id, execution_id, manifest_digest);


--
-- Name: execution_runtime_tool_manifest_t execution_runtime_tool_manifest_t_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.execution_runtime_tool_manifest_t
    ADD CONSTRAINT execution_runtime_tool_manifest_t_pkey PRIMARY KEY (host_id, manifest_id);


--
-- Name: execution_session_cleanup_request_t execution_session_cleanup_req_host_id_origin_service_id_ori_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.execution_session_cleanup_request_t
    ADD CONSTRAINT execution_session_cleanup_req_host_id_origin_service_id_ori_key UNIQUE (host_id, origin_service_id, origin_instance_id, idempotency_key);


--
-- Name: execution_session_cleanup_request_t execution_session_cleanup_request_t_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.execution_session_cleanup_request_t
    ADD CONSTRAINT execution_session_cleanup_request_t_pkey PRIMARY KEY (host_id, cleanup_request_id);


--
-- Name: execution_session_t execution_session_t_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.execution_session_t
    ADD CONSTRAINT execution_session_t_pkey PRIMARY KEY (host_id, execution_session_id);


--
-- Name: runner_backend_t runner_backend_t_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.runner_backend_t
    ADD CONSTRAINT runner_backend_t_pkey PRIMARY KEY (host_id, session_id, backend_id);


--
-- Name: runner_scheduling_request_t runner_scheduling_request_t_host_id_origin_service_id_origi_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.runner_scheduling_request_t
    ADD CONSTRAINT runner_scheduling_request_t_host_id_origin_service_id_origi_key UNIQUE (host_id, origin_service_id, origin_instance_id, idempotency_key);


--
-- Name: runner_scheduling_request_t runner_scheduling_request_t_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.runner_scheduling_request_t
    ADD CONSTRAINT runner_scheduling_request_t_pkey PRIMARY KEY (host_id, request_id);


--
-- Name: runner_session_t runner_session_t_host_id_runner_id_connection_generation_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.runner_session_t
    ADD CONSTRAINT runner_session_t_host_id_runner_id_connection_generation_key UNIQUE (host_id, runner_id, connection_generation);


--
-- Name: runner_session_t runner_session_t_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.runner_session_t
    ADD CONSTRAINT runner_session_t_pkey PRIMARY KEY (host_id, session_id);


--
-- Name: execution_attempt_active_lease_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX execution_attempt_active_lease_idx ON execution_ops.execution_attempt_t USING btree (lease_deadline_ts) WHERE ((state)::text = ANY (ARRAY[('CREATED'::character varying)::text, ('LEASED'::character varying)::text, ('STARTED'::character varying)::text]));


--
-- Name: execution_attempt_cleanup_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX execution_attempt_cleanup_idx ON execution_ops.execution_attempt_t USING btree (cleanup_state, updated_ts) WHERE ((cleanup_state)::text = ANY (ARRAY[('REQUIRED'::character varying)::text, ('IN_PROGRESS'::character varying)::text, ('FAILED'::character varying)::text]));


--
-- Name: execution_attempt_origin_result_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX execution_attempt_origin_result_idx ON execution_ops.execution_attempt_t USING btree (origin_service_id, origin_instance_id, terminal_ts, execution_id) WHERE ((terminal_ts IS NOT NULL) AND (accepted_by_origin_ts IS NULL));


--
-- Name: execution_credential_grant_expiry_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX execution_credential_grant_expiry_idx ON execution_ops.execution_credential_grant_audit_t USING btree (expires_ts) WHERE (revoked_ts IS NULL);


--
-- Name: execution_fixed_action_idempotency_uk; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX execution_fixed_action_idempotency_uk ON execution_ops.execution_fixed_action_t USING btree (host_id, action_kind, idempotency_key) WHERE (idempotency_key IS NOT NULL);


--
-- Name: execution_fixed_action_reconcile_due_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX execution_fixed_action_reconcile_due_idx ON execution_ops.execution_fixed_action_t USING btree (next_reconcile_ts, updated_ts) WHERE ((state)::text = ANY (ARRAY[('RUNNING'::character varying)::text, ('UNKNOWN'::character varying)::text]));


--
-- Name: execution_runtime_audit_execution_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX execution_runtime_audit_execution_idx ON execution_ops.execution_runtime_audit_t USING btree (host_id, execution_id, audit_id) WHERE (execution_id IS NOT NULL);


--
-- Name: execution_runtime_audit_subject_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX execution_runtime_audit_subject_idx ON execution_ops.execution_runtime_audit_t USING btree (host_id, subject_kind, subject_id, audit_id);


--
-- Name: execution_session_cleanup_active_uk; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX execution_session_cleanup_active_uk ON execution_ops.execution_session_cleanup_request_t USING btree (host_id, execution_session_id) WHERE ((state)::text = ANY (ARRAY[('PENDING'::character varying)::text, ('FENCED'::character varying)::text, ('DELIVERED'::character varying)::text]));


--
-- Name: execution_session_cleanup_due_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX execution_session_cleanup_due_idx ON execution_ops.execution_session_cleanup_request_t USING btree (state, cleanup_deadline_ts) WHERE ((state)::text = ANY (ARRAY[('PENDING'::character varying)::text, ('FENCED'::character varying)::text, ('DELIVERED'::character varying)::text]));


--
-- Name: execution_session_expiry_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX execution_session_expiry_idx ON execution_ops.execution_session_t USING btree (effective_expires_ts, state) WHERE ((state)::text <> ALL (ARRAY[('CLEANED'::character varying)::text, ('FAILED'::character varying)::text]));


--
-- Name: execution_session_hold_expiry_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX execution_session_hold_expiry_idx ON execution_ops.execution_session_t USING btree (hold_until_ts) WHERE ((state)::text = 'IDLE_APPROVAL_HOLD'::text);


--
-- Name: runner_backend_capacity_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX runner_backend_capacity_idx ON execution_ops.runner_backend_t USING btree (host_id, health, boundary_class, available_slots DESC) WHERE (available_slots > 0);


--
-- Name: runner_request_active_subject_uk; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX runner_request_active_subject_uk ON execution_ops.runner_scheduling_request_t USING btree (host_id, origin_service_id, origin_instance_id, subject_kind, subject_id) WHERE ((state)::text = ANY (ARRAY[('PENDING_CAPACITY'::character varying)::text, ('RESERVED'::character varying)::text, ('ATTEMPT_CREATED'::character varying)::text, ('LEASED'::character varying)::text]));


--
-- Name: runner_request_approval_uk; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX runner_request_approval_uk ON execution_ops.runner_scheduling_request_t USING btree (host_id, approval_id) WHERE (approval_id IS NOT NULL);


--
-- Name: runner_request_fair_queue_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX runner_request_fair_queue_idx ON execution_ops.runner_scheduling_request_t USING btree (state, not_before_ts, priority DESC, queue_sequence) WHERE ((state)::text = 'PENDING_CAPACITY'::text);


--
-- Name: runner_request_reservation_expiry_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX runner_request_reservation_expiry_idx ON execution_ops.runner_scheduling_request_t USING btree (reservation_expires_ts) WHERE ((state)::text = 'RESERVED'::text);


--
-- Name: runner_session_live_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX runner_session_live_idx ON execution_ops.runner_session_t USING btree (host_id, status, drain_state, heartbeat_ts DESC);


--
-- Name: execution_attempt_t execution_attempt_result_notify; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER execution_attempt_result_notify AFTER INSERT OR UPDATE OF state, terminal_ts ON execution_ops.execution_attempt_t FOR EACH ROW EXECUTE FUNCTION execution_ops.notify_execution_result_ready_v1();


--
-- Name: execution_runtime_audit_t execution_runtime_audit_no_update; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER execution_runtime_audit_no_update BEFORE DELETE OR UPDATE ON execution_ops.execution_runtime_audit_t FOR EACH ROW EXECUTE FUNCTION execution_ops.execution_runtime_audit_append_only();


--
-- Name: runner_scheduling_request_t runner_request_policy_snapshot_v1; Type: TRIGGER; Schema: public; Owner: -
--

CREATE CONSTRAINT TRIGGER runner_request_projection_v1 AFTER INSERT OR UPDATE OF origin_kind, host_id, policy_snapshot_id, policy_digest, normalized_requirements, origin_reference_digest ON execution_ops.runner_scheduling_request_t DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION execution_ops.validate_runner_request_projection_v1();


--
-- Name: execution_attempt_t execution_attempt_t_host_id_request_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.execution_attempt_t
    ADD CONSTRAINT execution_attempt_t_host_id_request_id_fkey FOREIGN KEY (host_id, request_id) REFERENCES execution_ops.runner_scheduling_request_t(host_id, request_id) ON DELETE RESTRICT;


--
-- Name: execution_attempt_t execution_attempt_t_host_id_runner_session_id_backend_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.execution_attempt_t
    ADD CONSTRAINT execution_attempt_t_host_id_runner_session_id_backend_id_fkey FOREIGN KEY (host_id, runner_session_id, backend_id) REFERENCES execution_ops.runner_backend_t(host_id, session_id, backend_id) ON DELETE RESTRICT;


--
-- Name: execution_credential_grant_audit_t execution_credential_grant_audit_t_host_id_execution_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.execution_credential_grant_audit_t
    ADD CONSTRAINT execution_credential_grant_audit_t_host_id_execution_id_fkey FOREIGN KEY (host_id, execution_id) REFERENCES execution_ops.execution_attempt_t(host_id, execution_id) ON DELETE RESTRICT;


--
-- Name: execution_fixed_action_t execution_fixed_action_t_host_id_execution_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.execution_fixed_action_t
    ADD CONSTRAINT execution_fixed_action_t_host_id_execution_id_fkey FOREIGN KEY (host_id, execution_id) REFERENCES execution_ops.execution_attempt_t(host_id, execution_id) ON DELETE RESTRICT;


--
-- Name: execution_input_t execution_input_t_host_id_execution_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.execution_input_t
    ADD CONSTRAINT execution_input_t_host_id_execution_id_fkey FOREIGN KEY (host_id, execution_id) REFERENCES execution_ops.execution_attempt_t(host_id, execution_id) ON DELETE RESTRICT;


--
-- Name: execution_input_t execution_input_t_host_id_execution_session_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.execution_input_t
    ADD CONSTRAINT execution_input_t_host_id_execution_session_id_fkey FOREIGN KEY (host_id, execution_session_id) REFERENCES execution_ops.execution_session_t(host_id, execution_session_id) ON DELETE RESTRICT;


--
-- Name: execution_input_t execution_input_t_host_id_request_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.execution_input_t
    ADD CONSTRAINT execution_input_t_host_id_request_id_fkey FOREIGN KEY (host_id, request_id) REFERENCES execution_ops.runner_scheduling_request_t(host_id, request_id) ON DELETE RESTRICT;


--
-- Name: execution_provenance_t execution_provenance_t_host_id_execution_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.execution_provenance_t
    ADD CONSTRAINT execution_provenance_t_host_id_execution_id_fkey FOREIGN KEY (host_id, execution_id) REFERENCES execution_ops.execution_attempt_t(host_id, execution_id) ON DELETE RESTRICT;


--
-- Name: execution_runtime_tool_manifest_t execution_runtime_tool_manifest_t_host_id_execution_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.execution_runtime_tool_manifest_t
    ADD CONSTRAINT execution_runtime_tool_manifest_t_host_id_execution_id_fkey FOREIGN KEY (host_id, execution_id) REFERENCES execution_ops.execution_attempt_t(host_id, execution_id) ON DELETE RESTRICT;


--
-- Name: execution_session_cleanup_request_t execution_session_cleanup_req_host_id_execution_session_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.execution_session_cleanup_request_t
    ADD CONSTRAINT execution_session_cleanup_req_host_id_execution_session_id_fkey FOREIGN KEY (host_id, execution_session_id) REFERENCES execution_ops.execution_session_t(host_id, execution_session_id) ON DELETE RESTRICT;


--
-- Name: execution_session_t execution_session_t_host_id_runner_session_id_backend_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.execution_session_t
    ADD CONSTRAINT execution_session_t_host_id_runner_session_id_backend_id_fkey FOREIGN KEY (host_id, runner_session_id, backend_id) REFERENCES execution_ops.runner_backend_t(host_id, session_id, backend_id) ON DELETE RESTRICT;


--
-- Name: runner_backend_t runner_backend_t_host_id_session_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.runner_backend_t
    ADD CONSTRAINT runner_backend_t_host_id_session_id_fkey FOREIGN KEY (host_id, session_id) REFERENCES execution_ops.runner_session_t(host_id, session_id) ON DELETE CASCADE;


--
-- Name: runner_scheduling_request_t runner_scheduling_request_t_host_id_selected_runner_sessio_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY execution_ops.runner_scheduling_request_t
    ADD CONSTRAINT runner_scheduling_request_t_host_id_selected_runner_sessio_fkey FOREIGN KEY (host_id, selected_runner_session_id, selected_backend_id) REFERENCES execution_ops.runner_backend_t(host_id, session_id, backend_id) ON DELETE RESTRICT;

RESET ROLE;

REVOKE ALL ON ALL TABLES IN SCHEMA execution_ops FROM PUBLIC;
REVOKE ALL ON ALL SEQUENCES IN SCHEMA execution_ops FROM PUBLIC;
REVOKE ALL ON ALL FUNCTIONS IN SCHEMA execution_ops FROM PUBLIC;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA execution_ops
    TO operations_execution_runtime;
GRANT USAGE, SELECT, UPDATE ON ALL SEQUENCES IN SCHEMA execution_ops
    TO operations_execution_runtime;
