-- Phase 4: authoritative Agent and embedded-memory state in
-- operations.agent_ops. The schema exists from the Phase 1 metadata bundle.
-- Development deployments reset and reseed; this migration copies no rows
-- from Config Server and creates no cross-database compatibility path.

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

CREATE EXTENSION IF NOT EXISTS vector WITH SCHEMA public;

GRANT SELECT ON operational_meta.operational_schema_migration_t
    TO operations_agent_runtime;

GRANT USAGE ON SCHEMA operational_meta, public
    TO operations_agent_runtime;

SET ROLE operations_agent_migrator;

--
-- Name: agent_action_attempt_t; Type: TABLE; Schema: agent_ops; Owner: -
--

CREATE TABLE agent_ops.agent_action_attempt_t (
    host_id uuid NOT NULL,
    action_attempt_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    logical_action_id uuid NOT NULL,
    attempt_number integer NOT NULL,
    stable_tool_ref uuid NOT NULL,
    model_alias character varying(126) NOT NULL,
    placement character varying(16) NOT NULL,
    schema_digest character varying(71) NOT NULL,
    policy_digest character varying(71) NOT NULL,
    argument_digest character varying(71) NOT NULL,
    effect_class character varying(32) NOT NULL,
    state character varying(32) NOT NULL,
    approval_id uuid,
    execution_attempt_id uuid,
    execution_reference_digest character varying(128),
    superseded_action_attempt_id uuid,
    gateway_request_id uuid,
    gateway_token_id uuid,
    runtime_adapter_id character varying(126),
    runtime_adapter_version character varying(64),
    runtime_capability_digest character varying(71),
    result jsonb,
    result_digest character varying(71),
    origin_accepted_ts timestamp with time zone,
    created_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    CONSTRAINT agent_action_attempt_t_attempt_number_check CHECK ((attempt_number > 0)),
    CONSTRAINT agent_action_attempt_t_placement_check CHECK (((placement)::text = ANY (ARRAY[('gateway'::character varying)::text, ('runner'::character varying)::text, ('workflow'::character varying)::text, ('fixed'::character varying)::text]))),
    CONSTRAINT agent_action_attempt_t_state_check CHECK (((state)::text = ANY (ARRAY[('PROPOSED'::character varying)::text, ('WAITING_APPROVAL'::character varying)::text, ('READY'::character varying)::text, ('DISPATCHED'::character varying)::text, ('RUNNING'::character varying)::text, ('APPROVAL_REQUIRED'::character varying)::text, ('SUCCEEDED'::character varying)::text, ('FAILED'::character varying)::text, ('CANCELLED'::character varying)::text, ('UNKNOWN'::character varying)::text, ('OPERATOR_REQUIRED'::character varying)::text, ('ACCEPTED'::character varying)::text])))
);


--
-- Name: TABLE agent_action_attempt_t; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON TABLE agent_ops.agent_action_attempt_t IS 'Stores agent action attempt records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN agent_action_attempt_t.host_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_action_attempt_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN agent_action_attempt_t.action_attempt_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_action_attempt_t.action_attempt_id IS 'Identifier for the related action attempt.';


--
-- Name: COLUMN agent_action_attempt_t.turn_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_action_attempt_t.turn_id IS 'Identifier for the related turn.';


--
-- Name: COLUMN agent_action_attempt_t.logical_action_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_action_attempt_t.logical_action_id IS 'Identifier for the related logical action.';


--
-- Name: COLUMN agent_action_attempt_t.attempt_number; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_action_attempt_t.attempt_number IS 'Attempt Number value for this agent action attempt record.';


--
-- Name: COLUMN agent_action_attempt_t.stable_tool_ref; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_action_attempt_t.stable_tool_ref IS 'Stable Tool Ref value for this agent action attempt record.';


--
-- Name: COLUMN agent_action_attempt_t.model_alias; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_action_attempt_t.model_alias IS 'Model Alias value for this agent action attempt record.';


--
-- Name: COLUMN agent_action_attempt_t.placement; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_action_attempt_t.placement IS 'Placement value for this agent action attempt record.';


--
-- Name: COLUMN agent_action_attempt_t.schema_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_action_attempt_t.schema_digest IS 'Integrity digest for schema.';


--
-- Name: COLUMN agent_action_attempt_t.policy_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_action_attempt_t.policy_digest IS 'Integrity digest for policy.';


--
-- Name: COLUMN agent_action_attempt_t.argument_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_action_attempt_t.argument_digest IS 'Integrity digest for argument.';


--
-- Name: COLUMN agent_action_attempt_t.effect_class; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_action_attempt_t.effect_class IS 'Effect Class value for this agent action attempt record.';


--
-- Name: COLUMN agent_action_attempt_t.state; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_action_attempt_t.state IS 'State value for this agent action attempt record.';


--
-- Name: COLUMN agent_action_attempt_t.approval_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_action_attempt_t.approval_id IS 'Identifier for the related approval.';


--
-- Name: COLUMN agent_action_attempt_t.execution_attempt_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_action_attempt_t.execution_attempt_id IS 'Identifier for the related execution attempt.';


--
-- Name: COLUMN agent_action_attempt_t.execution_reference_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_action_attempt_t.execution_reference_digest IS 'Digest of the authenticated execution reference accepted by Agent reconciliation.';


--
-- Name: COLUMN agent_action_attempt_t.superseded_action_attempt_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_action_attempt_t.superseded_action_attempt_id IS 'Identifier for the related superseded action attempt.';


--
-- Name: COLUMN agent_action_attempt_t.gateway_request_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_action_attempt_t.gateway_request_id IS 'Identifier for the related gateway request.';


--
-- Name: COLUMN agent_action_attempt_t.gateway_token_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_action_attempt_t.gateway_token_id IS 'Identifier for the related gateway token.';


--
-- Name: COLUMN agent_action_attempt_t.runtime_adapter_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_action_attempt_t.runtime_adapter_id IS 'Identifier for the related runtime adapter.';


--
-- Name: COLUMN agent_action_attempt_t.runtime_adapter_version; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_action_attempt_t.runtime_adapter_version IS 'Version value for runtime adapter.';


--
-- Name: COLUMN agent_action_attempt_t.runtime_capability_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_action_attempt_t.runtime_capability_digest IS 'Integrity digest for runtime capability.';


--
-- Name: COLUMN agent_action_attempt_t.result; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_action_attempt_t.result IS 'Result value for this agent action attempt record.';


--
-- Name: COLUMN agent_action_attempt_t.result_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_action_attempt_t.result_digest IS 'Integrity digest for result.';


--
-- Name: COLUMN agent_action_attempt_t.origin_accepted_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_action_attempt_t.origin_accepted_ts IS 'Timestamp for the origin accepted event or state.';


--
-- Name: COLUMN agent_action_attempt_t.created_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_action_attempt_t.created_ts IS 'Timestamp for the created event or state.';


--
-- Name: COLUMN agent_action_attempt_t.updated_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_action_attempt_t.updated_ts IS 'Timestamp for the updated event or state.';


--
-- Name: agent_approval_t; Type: TABLE; Schema: agent_ops; Owner: -
--

CREATE TABLE agent_ops.agent_approval_t (
    host_id uuid NOT NULL,
    approval_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    logical_action_id uuid NOT NULL,
    subject_digest character varying(71) NOT NULL,
    input_digest character varying(71) NOT NULL,
    policy_digest character varying(71) NOT NULL,
    approver_scope jsonb NOT NULL,
    state character varying(16) DEFAULT 'REQUESTED'::character varying NOT NULL,
    nonce_digest character varying(71) NOT NULL,
    expires_ts timestamp with time zone NOT NULL,
    decision_actor character varying(255),
    decision_ts timestamp with time zone,
    decision_reason text,
    consumed_action_attempt_id uuid,
    consumed_execution_attempt_id uuid,
    consumed_execution_reference_digest character varying(128),
    created_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    CONSTRAINT agent_approval_t_check CHECK (((((state)::text = 'REQUESTED'::text) AND (decision_ts IS NULL)) OR (((state)::text <> 'REQUESTED'::text) AND (decision_ts IS NOT NULL)))),
    CONSTRAINT agent_approval_t_state_check CHECK (((state)::text = ANY (ARRAY[('REQUESTED'::character varying)::text, ('APPROVED'::character varying)::text, ('REJECTED'::character varying)::text, ('EXPIRED'::character varying)::text, ('REVOKED'::character varying)::text])))
);


--
-- Name: TABLE agent_approval_t; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON TABLE agent_ops.agent_approval_t IS 'Stores agent approval records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN agent_approval_t.host_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_approval_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN agent_approval_t.approval_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_approval_t.approval_id IS 'Identifier for the related approval.';


--
-- Name: COLUMN agent_approval_t.turn_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_approval_t.turn_id IS 'Identifier for the related turn.';


--
-- Name: COLUMN agent_approval_t.logical_action_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_approval_t.logical_action_id IS 'Identifier for the related logical action.';


--
-- Name: COLUMN agent_approval_t.subject_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_approval_t.subject_digest IS 'Integrity digest for subject.';


--
-- Name: COLUMN agent_approval_t.input_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_approval_t.input_digest IS 'Integrity digest for input.';


--
-- Name: COLUMN agent_approval_t.policy_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_approval_t.policy_digest IS 'Integrity digest for policy.';


--
-- Name: COLUMN agent_approval_t.approver_scope; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_approval_t.approver_scope IS 'Approver Scope value for this agent approval record.';


--
-- Name: COLUMN agent_approval_t.state; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_approval_t.state IS 'State value for this agent approval record.';


--
-- Name: COLUMN agent_approval_t.nonce_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_approval_t.nonce_digest IS 'Integrity digest for nonce.';


--
-- Name: COLUMN agent_approval_t.expires_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_approval_t.expires_ts IS 'Timestamp for the expires event or state.';


--
-- Name: COLUMN agent_approval_t.decision_actor; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_approval_t.decision_actor IS 'Decision Actor value for this agent approval record.';


--
-- Name: COLUMN agent_approval_t.decision_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_approval_t.decision_ts IS 'Timestamp for the decision event or state.';


--
-- Name: COLUMN agent_approval_t.decision_reason; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_approval_t.decision_reason IS 'Decision Reason value for this agent approval record.';


--
-- Name: COLUMN agent_approval_t.consumed_action_attempt_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_approval_t.consumed_action_attempt_id IS 'Identifier for the related consumed action attempt.';


--
-- Name: COLUMN agent_approval_t.consumed_execution_attempt_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_approval_t.consumed_execution_attempt_id IS 'Identifier for the related consumed execution attempt.';


--
-- Name: COLUMN agent_approval_t.consumed_execution_reference_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_approval_t.consumed_execution_reference_digest IS 'Digest of signed evidence binding approval consumption to an execution.';


--
-- Name: COLUMN agent_approval_t.created_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_approval_t.created_ts IS 'Timestamp for the created event or state.';


--
-- Name: agent_delegation_replay_t; Type: TABLE; Schema: agent_ops; Owner: -
--

CREATE TABLE agent_ops.agent_delegation_replay_t (
    host_id uuid NOT NULL,
    audience character varying(255) NOT NULL,
    replay_id uuid NOT NULL,
    token_id uuid NOT NULL,
    action_attempt_id uuid,
    issuer character varying(255) NOT NULL,
    gateway_instance character varying(255) NOT NULL,
    consumed_ts timestamp with time zone DEFAULT now() NOT NULL,
    expires_ts timestamp with time zone NOT NULL,
    CONSTRAINT agent_delegation_replay_t_check CHECK ((expires_ts > (consumed_ts - '00:00:30'::interval)))
);


--
-- Name: TABLE agent_delegation_replay_t; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON TABLE agent_ops.agent_delegation_replay_t IS 'Stores agent delegation replay records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN agent_delegation_replay_t.host_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_delegation_replay_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN agent_delegation_replay_t.audience; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_delegation_replay_t.audience IS 'Audience value for this agent delegation replay record.';


--
-- Name: COLUMN agent_delegation_replay_t.replay_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_delegation_replay_t.replay_id IS 'Identifier for the related replay.';


--
-- Name: COLUMN agent_delegation_replay_t.token_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_delegation_replay_t.token_id IS 'Identifier for the related token.';


--
-- Name: COLUMN agent_delegation_replay_t.action_attempt_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_delegation_replay_t.action_attempt_id IS 'Identifier for the related action attempt.';


--
-- Name: COLUMN agent_delegation_replay_t.issuer; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_delegation_replay_t.issuer IS 'Issuer value for this agent delegation replay record.';


--
-- Name: COLUMN agent_delegation_replay_t.gateway_instance; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_delegation_replay_t.gateway_instance IS 'Gateway Instance value for this agent delegation replay record.';


--
-- Name: COLUMN agent_delegation_replay_t.consumed_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_delegation_replay_t.consumed_ts IS 'Timestamp for the consumed event or state.';


--
-- Name: COLUMN agent_delegation_replay_t.expires_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_delegation_replay_t.expires_ts IS 'Timestamp for the expires event or state.';


--
-- Name: agent_execution_outbox_t; Type: TABLE; Schema: agent_ops; Owner: -
--

CREATE TABLE agent_ops.agent_execution_outbox_t (
    host_id uuid NOT NULL,
    dispatch_id uuid NOT NULL,
    request_id uuid NOT NULL,
    command_kind character varying(16) NOT NULL,
    command_payload jsonb NOT NULL,
    payload_digest character varying(71) NOT NULL,
    state character varying(16) DEFAULT 'PENDING'::character varying NOT NULL,
    attempt_count integer DEFAULT 0 NOT NULL,
    next_attempt_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    last_error character varying(512),
    dispatched_ts timestamp with time zone,
    created_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    CONSTRAINT agent_execution_outbox_t_command_check CHECK (((command_kind)::text = ANY (ARRAY['REQUEST'::text, 'CLEANUP'::text]))),
    CONSTRAINT agent_execution_outbox_t_digest_check CHECK (((payload_digest)::text ~ '^sha256:[0-9a-f]{64}$'::text)),
    CONSTRAINT agent_execution_outbox_t_payload_check CHECK ((jsonb_typeof(command_payload) = 'object'::text)),
    CONSTRAINT agent_execution_outbox_t_state_check CHECK (((state)::text = ANY (ARRAY['PENDING'::text, 'DISPATCHED'::text, 'DEAD'::text])))
);


--
-- Name: TABLE agent_execution_outbox_t; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON TABLE agent_ops.agent_execution_outbox_t IS 'Agent-owned durable handoff to the Controller execution API; Config Server execution tables are not authoritative.';


--
-- Name: agent_fixed_action_t; Type: TABLE; Schema: agent_ops; Owner: -
--

CREATE TABLE agent_ops.agent_fixed_action_t (
    host_id uuid NOT NULL,
    fixed_action_id uuid NOT NULL,
    action_kind character varying(32) NOT NULL,
    subject_kind character varying(32) NOT NULL,
    subject_id uuid NOT NULL,
    input_digest character varying(71) NOT NULL,
    target_digest character varying(71) NOT NULL,
    policy_digest character varying(71) NOT NULL,
    provenance_digest character varying(71) NOT NULL,
    approval_reference uuid NOT NULL,
    approval_nonce_digest character varying(71) NOT NULL,
    approval_expires_ts timestamp with time zone NOT NULL,
    state character varying(32) NOT NULL,
    credential_grant_id uuid,
    result_evidence jsonb,
    created_ts timestamp with time zone DEFAULT now() NOT NULL,
    updated_ts timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT agent_fixed_action_t_action_kind_check CHECK (((action_kind)::text = ANY (ARRAY[('CREATE_BRANCH'::character varying)::text, ('OPEN_PR'::character varying)::text, ('PUSH_COMMIT'::character varying)::text, ('PUBLISH'::character varying)::text, ('SIGN'::character varying)::text, ('DEPLOY'::character varying)::text, ('SEND_EMAIL'::character varying)::text, ('CREATE_EVENT'::character varying)::text, ('PAYMENT'::character varying)::text]))),
    CONSTRAINT agent_fixed_action_t_state_check CHECK (((state)::text = ANY (ARRAY[('REQUESTED'::character varying)::text, ('VALIDATED'::character varying)::text, ('RUNNING'::character varying)::text, ('SUCCEEDED'::character varying)::text, ('FAILED'::character varying)::text, ('REJECTED'::character varying)::text, ('REVOKED'::character varying)::text])))
);


--
-- Name: TABLE agent_fixed_action_t; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON TABLE agent_ops.agent_fixed_action_t IS 'Stores agent fixed action records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN agent_fixed_action_t.host_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_fixed_action_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN agent_fixed_action_t.fixed_action_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_fixed_action_t.fixed_action_id IS 'Identifier for the related fixed action.';


--
-- Name: COLUMN agent_fixed_action_t.action_kind; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_fixed_action_t.action_kind IS 'Action Kind value for this agent fixed action record.';


--
-- Name: COLUMN agent_fixed_action_t.subject_kind; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_fixed_action_t.subject_kind IS 'Subject Kind value for this agent fixed action record.';


--
-- Name: COLUMN agent_fixed_action_t.subject_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_fixed_action_t.subject_id IS 'Identifier for the related subject.';


--
-- Name: COLUMN agent_fixed_action_t.input_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_fixed_action_t.input_digest IS 'Integrity digest for input.';


--
-- Name: COLUMN agent_fixed_action_t.target_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_fixed_action_t.target_digest IS 'Integrity digest for target.';


--
-- Name: COLUMN agent_fixed_action_t.policy_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_fixed_action_t.policy_digest IS 'Integrity digest for policy.';


--
-- Name: COLUMN agent_fixed_action_t.provenance_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_fixed_action_t.provenance_digest IS 'Integrity digest for provenance.';


--
-- Name: COLUMN agent_fixed_action_t.approval_reference; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_fixed_action_t.approval_reference IS 'Approval Reference value for this agent fixed action record.';


--
-- Name: COLUMN agent_fixed_action_t.approval_nonce_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_fixed_action_t.approval_nonce_digest IS 'Integrity digest for approval nonce.';


--
-- Name: COLUMN agent_fixed_action_t.approval_expires_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_fixed_action_t.approval_expires_ts IS 'Timestamp for the approval expires event or state.';


--
-- Name: COLUMN agent_fixed_action_t.state; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_fixed_action_t.state IS 'State value for this agent fixed action record.';


--
-- Name: COLUMN agent_fixed_action_t.credential_grant_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_fixed_action_t.credential_grant_id IS 'Identifier for the related credential grant.';


--
-- Name: COLUMN agent_fixed_action_t.result_evidence; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_fixed_action_t.result_evidence IS 'Result Evidence value for this agent fixed action record.';


--
-- Name: COLUMN agent_fixed_action_t.created_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_fixed_action_t.created_ts IS 'Timestamp for the created event or state.';


--
-- Name: COLUMN agent_fixed_action_t.updated_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_fixed_action_t.updated_ts IS 'Timestamp for the updated event or state.';


--
-- Name: agent_job_t; Type: TABLE; Schema: agent_ops; Owner: -
--

CREATE TABLE agent_ops.agent_job_t (
    host_id uuid NOT NULL,
    job_id uuid NOT NULL,
    workflow_process_id uuid NOT NULL,
    workflow_task_id uuid NOT NULL,
    agent_def_id uuid NOT NULL,
    turn_id uuid,
    idempotency_key character varying(255) NOT NULL,
    input jsonb NOT NULL,
    input_schema_digest character varying(71) NOT NULL,
    output_schema jsonb NOT NULL,
    policy_digest character varying(71) NOT NULL,
    data_boundary_digest character varying(71) NOT NULL,
    deadline_ts timestamp with time zone NOT NULL,
    token_budget bigint NOT NULL,
    cost_budget_micros bigint NOT NULL,
    delegation_depth integer NOT NULL,
    state character varying(32) NOT NULL,
    public_output jsonb,
    error jsonb,
    created_ts timestamp with time zone DEFAULT now() NOT NULL,
    updated_ts timestamp with time zone DEFAULT now() NOT NULL,
    maximum_delegation_depth integer DEFAULT 4 NOT NULL,
    memory_mode character varying(16) DEFAULT 'ISOLATED'::character varying NOT NULL,
    cancellation_requested_ts timestamp with time zone,
    terminal_ts timestamp with time zone,
    CONSTRAINT agent_job_delegation_depth_ck CHECK (((delegation_depth >= 0) AND (maximum_delegation_depth >= 0) AND (delegation_depth <= maximum_delegation_depth))),
    CONSTRAINT agent_job_memory_mode_ck CHECK (((memory_mode)::text = ANY (ARRAY[('ISOLATED'::character varying)::text, ('SESSION'::character varying)::text]))),
    CONSTRAINT agent_job_t_state_check CHECK (((state)::text = ANY (ARRAY[('PENDING'::character varying)::text, ('TURN_CREATED'::character varying)::text, ('RUNNING'::character varying)::text, ('SUCCEEDED'::character varying)::text, ('FAILED'::character varying)::text, ('CANCELLED'::character varying)::text, ('UNKNOWN'::character varying)::text])))
);


--
-- Name: TABLE agent_job_t; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON TABLE agent_ops.agent_job_t IS 'Stores agent job records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN agent_job_t.host_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_job_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN agent_job_t.job_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_job_t.job_id IS 'Identifier for the related job.';


--
-- Name: COLUMN agent_job_t.workflow_process_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_job_t.workflow_process_id IS 'Identifier for the related workflow process.';


--
-- Name: COLUMN agent_job_t.workflow_task_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_job_t.workflow_task_id IS 'Identifier for the related workflow task.';


--
-- Name: COLUMN agent_job_t.agent_def_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_job_t.agent_def_id IS 'Identifier for the related agent def.';


--
-- Name: COLUMN agent_job_t.turn_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_job_t.turn_id IS 'Identifier for the related turn.';


--
-- Name: COLUMN agent_job_t.idempotency_key; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_job_t.idempotency_key IS 'Idempotency Key value for this agent job record.';


--
-- Name: COLUMN agent_job_t.input; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_job_t.input IS 'Input value for this agent job record.';


--
-- Name: COLUMN agent_job_t.input_schema_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_job_t.input_schema_digest IS 'Integrity digest for input schema.';


--
-- Name: COLUMN agent_job_t.output_schema; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_job_t.output_schema IS 'Output Schema value for this agent job record.';


--
-- Name: COLUMN agent_job_t.policy_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_job_t.policy_digest IS 'Integrity digest for policy.';


--
-- Name: COLUMN agent_job_t.data_boundary_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_job_t.data_boundary_digest IS 'Integrity digest for data boundary.';


--
-- Name: COLUMN agent_job_t.deadline_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_job_t.deadline_ts IS 'Timestamp for the deadline event or state.';


--
-- Name: COLUMN agent_job_t.token_budget; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_job_t.token_budget IS 'Token Budget value for this agent job record.';


--
-- Name: COLUMN agent_job_t.cost_budget_micros; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_job_t.cost_budget_micros IS 'Cost Budget Micros value for this agent job record.';


--
-- Name: COLUMN agent_job_t.delegation_depth; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_job_t.delegation_depth IS 'Delegation Depth value for this agent job record.';


--
-- Name: COLUMN agent_job_t.state; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_job_t.state IS 'State value for this agent job record.';


--
-- Name: COLUMN agent_job_t.public_output; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_job_t.public_output IS 'Public Output value for this agent job record.';


--
-- Name: COLUMN agent_job_t.error; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_job_t.error IS 'Error value for this agent job record.';


--
-- Name: COLUMN agent_job_t.created_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_job_t.created_ts IS 'Timestamp for the created event or state.';


--
-- Name: COLUMN agent_job_t.updated_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_job_t.updated_ts IS 'Timestamp for the updated event or state.';


--
-- Name: COLUMN agent_job_t.maximum_delegation_depth; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_job_t.maximum_delegation_depth IS 'Maximum Delegation Depth value for this agent job record.';


--
-- Name: COLUMN agent_job_t.memory_mode; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_job_t.memory_mode IS 'Memory Mode value for this agent job record.';


--
-- Name: COLUMN agent_job_t.cancellation_requested_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_job_t.cancellation_requested_ts IS 'Timestamp for the cancellation requested event or state.';


--
-- Name: COLUMN agent_job_t.terminal_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_job_t.terminal_ts IS 'Timestamp for the terminal event or state.';


--
-- Name: agent_memory_bank_t; Type: TABLE; Schema: agent_ops; Owner: -
--

CREATE TABLE agent_ops.agent_memory_bank_t (
    host_id uuid NOT NULL,
    bank_id uuid NOT NULL,
    agent_def_id uuid,
    user_id uuid,
    agent_definition_version bigint,
    agent_definition_digest character varying(128),
    user_identity_digest character varying(128),
    bank_name character varying(126) NOT NULL,
    disposition jsonb DEFAULT '{"empathy": 3, "literalism": 3, "skepticism": 3}'::jsonb NOT NULL,
    background text,
    aggregate_version bigint DEFAULT 1 NOT NULL,
    active boolean DEFAULT true,
    update_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP,
    update_user character varying(126) DEFAULT SESSION_USER
);


--
-- Name: TABLE agent_memory_bank_t; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON TABLE agent_ops.agent_memory_bank_t IS 'Stores agent memory bank records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN agent_memory_bank_t.host_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_bank_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN agent_memory_bank_t.bank_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_bank_t.bank_id IS 'Identifier for the related bank.';


--
-- Name: COLUMN agent_memory_bank_t.agent_def_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_bank_t.agent_def_id IS 'Identifier for the related agent def.';


--
-- Name: COLUMN agent_memory_bank_t.user_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_bank_t.user_id IS 'Identifier for the related user.';


--
-- Name: COLUMN agent_memory_bank_t.agent_definition_version; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_bank_t.agent_definition_version IS 'Agent definition version accepted when the bank was created.';


--
-- Name: COLUMN agent_memory_bank_t.agent_definition_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_bank_t.agent_definition_digest IS 'Agent definition digest accepted when the bank was created.';


--
-- Name: COLUMN agent_memory_bank_t.user_identity_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_bank_t.user_identity_digest IS 'Digest of the authenticated user identity accepted for the bank.';


--
-- Name: COLUMN agent_memory_bank_t.bank_name; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_bank_t.bank_name IS 'Bank Name value for this agent memory bank record.';


--
-- Name: COLUMN agent_memory_bank_t.disposition; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_bank_t.disposition IS 'Disposition value for this agent memory bank record.';


--
-- Name: COLUMN agent_memory_bank_t.background; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_bank_t.background IS 'Background value for this agent memory bank record.';


--
-- Name: COLUMN agent_memory_bank_t.aggregate_version; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_bank_t.aggregate_version IS 'Version value for aggregate.';


--
-- Name: COLUMN agent_memory_bank_t.active; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_bank_t.active IS 'Indicates whether this record is active.';


--
-- Name: COLUMN agent_memory_bank_t.update_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_bank_t.update_ts IS 'Timestamp when this record was last updated.';


--
-- Name: COLUMN agent_memory_bank_t.update_user; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_bank_t.update_user IS 'User or service principal that last updated this record.';


--
-- Name: agent_memory_doc_t; Type: TABLE; Schema: agent_ops; Owner: -
--

CREATE TABLE agent_ops.agent_memory_doc_t (
    host_id uuid NOT NULL,
    doc_id uuid NOT NULL,
    bank_id uuid NOT NULL,
    original_text text,
    content_hash text,
    aggregate_version bigint DEFAULT 1 NOT NULL,
    active boolean DEFAULT true,
    update_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP,
    update_user character varying(126) DEFAULT SESSION_USER
);


--
-- Name: TABLE agent_memory_doc_t; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON TABLE agent_ops.agent_memory_doc_t IS 'Stores agent memory doc records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN agent_memory_doc_t.host_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_doc_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN agent_memory_doc_t.doc_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_doc_t.doc_id IS 'Identifier for the related doc.';


--
-- Name: COLUMN agent_memory_doc_t.bank_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_doc_t.bank_id IS 'Identifier for the related bank.';


--
-- Name: COLUMN agent_memory_doc_t.original_text; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_doc_t.original_text IS 'Original Text value for this agent memory doc record.';


--
-- Name: COLUMN agent_memory_doc_t.content_hash; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_doc_t.content_hash IS 'Integrity digest for content.';


--
-- Name: COLUMN agent_memory_doc_t.aggregate_version; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_doc_t.aggregate_version IS 'Version value for aggregate.';


--
-- Name: COLUMN agent_memory_doc_t.active; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_doc_t.active IS 'Indicates whether this record is active.';


--
-- Name: COLUMN agent_memory_doc_t.update_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_doc_t.update_ts IS 'Timestamp when this record was last updated.';


--
-- Name: COLUMN agent_memory_doc_t.update_user; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_doc_t.update_user IS 'User or service principal that last updated this record.';


--
-- Name: agent_memory_entity_cooccur_t; Type: TABLE; Schema: agent_ops; Owner: -
--

CREATE TABLE agent_ops.agent_memory_entity_cooccur_t (
    host_id uuid NOT NULL,
    bank_id uuid NOT NULL,
    entity_id_1 uuid NOT NULL,
    entity_id_2 uuid NOT NULL,
    cooccur_count integer DEFAULT 1,
    last_cooccurred timestamp with time zone DEFAULT CURRENT_TIMESTAMP,
    aggregate_version bigint DEFAULT 1 NOT NULL,
    active boolean DEFAULT true,
    update_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP,
    update_user character varying(126) DEFAULT SESSION_USER,
    CONSTRAINT entity_cooccur_order_check CHECK ((entity_id_1 < entity_id_2))
);


--
-- Name: TABLE agent_memory_entity_cooccur_t; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON TABLE agent_ops.agent_memory_entity_cooccur_t IS 'Stores agent memory entity cooccur records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN agent_memory_entity_cooccur_t.host_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_entity_cooccur_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN agent_memory_entity_cooccur_t.bank_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_entity_cooccur_t.bank_id IS 'Identifier for the related bank.';


--
-- Name: COLUMN agent_memory_entity_cooccur_t.entity_id_1; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_entity_cooccur_t.entity_id_1 IS 'Entity Id 1 value for this agent memory entity cooccur record.';


--
-- Name: COLUMN agent_memory_entity_cooccur_t.entity_id_2; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_entity_cooccur_t.entity_id_2 IS 'Entity Id 2 value for this agent memory entity cooccur record.';


--
-- Name: COLUMN agent_memory_entity_cooccur_t.cooccur_count; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_entity_cooccur_t.cooccur_count IS 'Count of cooccur.';


--
-- Name: COLUMN agent_memory_entity_cooccur_t.last_cooccurred; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_entity_cooccur_t.last_cooccurred IS 'Last Cooccurred value for this agent memory entity cooccur record.';


--
-- Name: COLUMN agent_memory_entity_cooccur_t.aggregate_version; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_entity_cooccur_t.aggregate_version IS 'Version value for aggregate.';


--
-- Name: COLUMN agent_memory_entity_cooccur_t.active; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_entity_cooccur_t.active IS 'Indicates whether this record is active.';


--
-- Name: COLUMN agent_memory_entity_cooccur_t.update_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_entity_cooccur_t.update_ts IS 'Timestamp when this record was last updated.';


--
-- Name: COLUMN agent_memory_entity_cooccur_t.update_user; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_entity_cooccur_t.update_user IS 'User or service principal that last updated this record.';


--
-- Name: agent_memory_entity_t; Type: TABLE; Schema: agent_ops; Owner: -
--

CREATE TABLE agent_ops.agent_memory_entity_t (
    host_id uuid NOT NULL,
    entity_id uuid NOT NULL,
    bank_id uuid NOT NULL,
    user_id uuid,
    user_identity_digest character varying(128),
    canonical_name text NOT NULL,
    mention_count integer DEFAULT 1,
    metadata jsonb DEFAULT '{}'::jsonb,
    aggregate_version bigint DEFAULT 1 NOT NULL,
    active boolean DEFAULT true,
    update_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP,
    update_user character varying(126) DEFAULT SESSION_USER
);


--
-- Name: TABLE agent_memory_entity_t; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON TABLE agent_ops.agent_memory_entity_t IS 'Stores agent memory entity records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN agent_memory_entity_t.host_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_entity_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN agent_memory_entity_t.entity_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_entity_t.entity_id IS 'Identifier for the related entity.';


--
-- Name: COLUMN agent_memory_entity_t.bank_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_entity_t.bank_id IS 'Identifier for the related bank.';


--
-- Name: COLUMN agent_memory_entity_t.user_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_entity_t.user_id IS 'Identifier for the related user.';


--
-- Name: COLUMN agent_memory_entity_t.user_identity_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_entity_t.user_identity_digest IS 'Digest of the authenticated user identity accepted for the entity.';


--
-- Name: COLUMN agent_memory_entity_t.canonical_name; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_entity_t.canonical_name IS 'Canonical Name value for this agent memory entity record.';


--
-- Name: COLUMN agent_memory_entity_t.mention_count; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_entity_t.mention_count IS 'Count of mention.';


--
-- Name: COLUMN agent_memory_entity_t.metadata; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_entity_t.metadata IS 'Metadata value for this agent memory entity record.';


--
-- Name: COLUMN agent_memory_entity_t.aggregate_version; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_entity_t.aggregate_version IS 'Version value for aggregate.';


--
-- Name: COLUMN agent_memory_entity_t.active; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_entity_t.active IS 'Indicates whether this record is active.';


--
-- Name: COLUMN agent_memory_entity_t.update_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_entity_t.update_ts IS 'Timestamp when this record was last updated.';


--
-- Name: COLUMN agent_memory_entity_t.update_user; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_entity_t.update_user IS 'User or service principal that last updated this record.';


--
-- Name: agent_memory_link_t; Type: TABLE; Schema: agent_ops; Owner: -
--

CREATE TABLE agent_ops.agent_memory_link_t (
    host_id uuid NOT NULL,
    bank_id uuid NOT NULL,
    from_unit_id uuid NOT NULL,
    to_unit_id uuid NOT NULL,
    link_type character varying(32) NOT NULL,
    weight double precision DEFAULT 1.0 NOT NULL,
    aggregate_version bigint DEFAULT 1 NOT NULL,
    active boolean DEFAULT true,
    update_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP,
    update_user character varying(126) DEFAULT SESSION_USER,
    CONSTRAINT memory_links_type_check CHECK (((link_type)::text = ANY (ARRAY[('temporal'::character varying)::text, ('semantic'::character varying)::text, ('entity'::character varying)::text, ('causes'::character varying)::text, ('caused_by'::character varying)::text, ('enables'::character varying)::text, ('prevents'::character varying)::text])))
);


--
-- Name: TABLE agent_memory_link_t; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON TABLE agent_ops.agent_memory_link_t IS 'Stores agent memory link records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN agent_memory_link_t.host_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_link_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN agent_memory_link_t.bank_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_link_t.bank_id IS 'Identifier for the related bank.';


--
-- Name: COLUMN agent_memory_link_t.from_unit_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_link_t.from_unit_id IS 'Identifier for the related from unit.';


--
-- Name: COLUMN agent_memory_link_t.to_unit_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_link_t.to_unit_id IS 'Identifier for the related to unit.';


--
-- Name: COLUMN agent_memory_link_t.link_type; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_link_t.link_type IS 'Link Type value for this agent memory link record.';


--
-- Name: COLUMN agent_memory_link_t.weight; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_link_t.weight IS 'Weight value for this agent memory link record.';


--
-- Name: COLUMN agent_memory_link_t.aggregate_version; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_link_t.aggregate_version IS 'Version value for aggregate.';


--
-- Name: COLUMN agent_memory_link_t.active; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_link_t.active IS 'Indicates whether this record is active.';


--
-- Name: COLUMN agent_memory_link_t.update_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_link_t.update_ts IS 'Timestamp when this record was last updated.';


--
-- Name: COLUMN agent_memory_link_t.update_user; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_link_t.update_user IS 'User or service principal that last updated this record.';


--
-- Name: agent_memory_reflection_t; Type: TABLE; Schema: agent_ops; Owner: -
--

CREATE TABLE agent_ops.agent_memory_reflection_t (
    host_id uuid NOT NULL,
    reflection_id uuid NOT NULL,
    bank_id uuid NOT NULL,
    content text NOT NULL,
    embedding public.vector(384),
    aggregate_version bigint DEFAULT 1 NOT NULL,
    active boolean DEFAULT true,
    update_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP,
    update_user character varying(126) DEFAULT SESSION_USER
);


--
-- Name: TABLE agent_memory_reflection_t; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON TABLE agent_ops.agent_memory_reflection_t IS 'Stores agent memory reflection records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN agent_memory_reflection_t.host_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_reflection_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN agent_memory_reflection_t.reflection_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_reflection_t.reflection_id IS 'Identifier for the related reflection.';


--
-- Name: COLUMN agent_memory_reflection_t.bank_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_reflection_t.bank_id IS 'Identifier for the related bank.';


--
-- Name: COLUMN agent_memory_reflection_t.content; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_reflection_t.content IS 'Content value for this agent memory reflection record.';


--
-- Name: COLUMN agent_memory_reflection_t.embedding; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_reflection_t.embedding IS 'Embedding value for this agent memory reflection record.';


--
-- Name: COLUMN agent_memory_reflection_t.aggregate_version; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_reflection_t.aggregate_version IS 'Version value for aggregate.';


--
-- Name: COLUMN agent_memory_reflection_t.active; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_reflection_t.active IS 'Indicates whether this record is active.';


--
-- Name: COLUMN agent_memory_reflection_t.update_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_reflection_t.update_ts IS 'Timestamp when this record was last updated.';


--
-- Name: COLUMN agent_memory_reflection_t.update_user; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_reflection_t.update_user IS 'User or service principal that last updated this record.';


--
-- Name: agent_memory_unit_entity_t; Type: TABLE; Schema: agent_ops; Owner: -
--

CREATE TABLE agent_ops.agent_memory_unit_entity_t (
    host_id uuid NOT NULL,
    bank_id uuid NOT NULL,
    unit_id uuid NOT NULL,
    entity_id uuid NOT NULL
);


--
-- Name: TABLE agent_memory_unit_entity_t; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON TABLE agent_ops.agent_memory_unit_entity_t IS 'Stores agent memory unit entity records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN agent_memory_unit_entity_t.host_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_unit_entity_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN agent_memory_unit_entity_t.bank_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_unit_entity_t.bank_id IS 'Identifier for the related bank.';


--
-- Name: COLUMN agent_memory_unit_entity_t.unit_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_unit_entity_t.unit_id IS 'Identifier for the related unit.';


--
-- Name: COLUMN agent_memory_unit_entity_t.entity_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_unit_entity_t.entity_id IS 'Identifier for the related entity.';


--
-- Name: agent_memory_unit_t; Type: TABLE; Schema: agent_ops; Owner: -
--

CREATE TABLE agent_ops.agent_memory_unit_t (
    host_id uuid NOT NULL,
    unit_id uuid NOT NULL,
    bank_id uuid NOT NULL,
    doc_id uuid,
    content text NOT NULL,
    embedding public.vector(384),
    context text,
    event_date timestamp with time zone DEFAULT now() NOT NULL,
    occurred_start timestamp with time zone,
    occurred_end timestamp with time zone,
    mentioned_at timestamp with time zone,
    fact_type character varying(32) DEFAULT 'world'::character varying NOT NULL,
    metadata jsonb DEFAULT '{}'::jsonb,
    proof_count integer DEFAULT 1,
    source_memory_ids uuid[] DEFAULT ARRAY[]::uuid[],
    aggregate_version bigint DEFAULT 1 NOT NULL,
    active boolean DEFAULT true,
    update_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP,
    update_user character varying(126) DEFAULT SESSION_USER,
    CONSTRAINT agent_memory_unit_t_fact_type_check CHECK (((fact_type)::text = ANY (ARRAY[('world'::character varying)::text, ('experience'::character varying)::text, ('opinion'::character varying)::text, ('observation'::character varying)::text, ('mental_model'::character varying)::text])))
);


--
-- Name: TABLE agent_memory_unit_t; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON TABLE agent_ops.agent_memory_unit_t IS 'Stores agent memory unit records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN agent_memory_unit_t.host_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_unit_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN agent_memory_unit_t.unit_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_unit_t.unit_id IS 'Identifier for the related unit.';


--
-- Name: COLUMN agent_memory_unit_t.bank_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_unit_t.bank_id IS 'Identifier for the related bank.';


--
-- Name: COLUMN agent_memory_unit_t.doc_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_unit_t.doc_id IS 'Identifier for the related doc.';


--
-- Name: COLUMN agent_memory_unit_t.content; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_unit_t.content IS 'Content value for this agent memory unit record.';


--
-- Name: COLUMN agent_memory_unit_t.embedding; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_unit_t.embedding IS 'Embedding value for this agent memory unit record.';


--
-- Name: COLUMN agent_memory_unit_t.context; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_unit_t.context IS 'Context value for this agent memory unit record.';


--
-- Name: COLUMN agent_memory_unit_t.event_date; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_unit_t.event_date IS 'Event Date value for this agent memory unit record.';


--
-- Name: COLUMN agent_memory_unit_t.occurred_start; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_unit_t.occurred_start IS 'Occurred Start value for this agent memory unit record.';


--
-- Name: COLUMN agent_memory_unit_t.occurred_end; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_unit_t.occurred_end IS 'Occurred End value for this agent memory unit record.';


--
-- Name: COLUMN agent_memory_unit_t.mentioned_at; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_unit_t.mentioned_at IS 'Timestamp when mentioned occurred.';


--
-- Name: COLUMN agent_memory_unit_t.fact_type; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_unit_t.fact_type IS 'Fact Type value for this agent memory unit record.';


--
-- Name: COLUMN agent_memory_unit_t.metadata; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_unit_t.metadata IS 'Metadata value for this agent memory unit record.';


--
-- Name: COLUMN agent_memory_unit_t.proof_count; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_unit_t.proof_count IS 'Count of proof.';


--
-- Name: COLUMN agent_memory_unit_t.source_memory_ids; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_unit_t.source_memory_ids IS 'Source Memory Ids value for this agent memory unit record.';


--
-- Name: COLUMN agent_memory_unit_t.aggregate_version; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_unit_t.aggregate_version IS 'Version value for aggregate.';


--
-- Name: COLUMN agent_memory_unit_t.active; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_unit_t.active IS 'Indicates whether this record is active.';


--
-- Name: COLUMN agent_memory_unit_t.update_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_unit_t.update_ts IS 'Timestamp when this record was last updated.';


--
-- Name: COLUMN agent_memory_unit_t.update_user; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_memory_unit_t.update_user IS 'User or service principal that last updated this record.';


--
-- Name: agent_policy_snapshot_t; Type: TABLE; Schema: agent_ops; Owner: -
--

CREATE TABLE agent_ops.agent_policy_snapshot_t (
    host_id uuid NOT NULL,
    policy_snapshot_id uuid NOT NULL,
    agent_def_id uuid NOT NULL,
    agent_definition_version bigint,
    agent_publication_id uuid,
    agent_content_digest character varying(128),
    definition_digest character varying(71) NOT NULL,
    product_profile_digest character varying(71) NOT NULL,
    model_digest character varying(71) NOT NULL,
    catalog_digest character varying(71) NOT NULL,
    memory_digest character varying(71) NOT NULL,
    execution_digest character varying(71) NOT NULL,
    channel_digest character varying(71) NOT NULL,
    data_boundary_digest character varying(71) NOT NULL,
    resolved_snapshot jsonb NOT NULL,
    policy_digest character varying(71) NOT NULL,
    revoked_ts timestamp with time zone,
    created_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL
);


--
-- Name: TABLE agent_policy_snapshot_t; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON TABLE agent_ops.agent_policy_snapshot_t IS 'Stores agent policy snapshot records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN agent_policy_snapshot_t.host_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_policy_snapshot_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN agent_policy_snapshot_t.policy_snapshot_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_policy_snapshot_t.policy_snapshot_id IS 'Identifier for the related policy snapshot.';


--
-- Name: COLUMN agent_policy_snapshot_t.agent_def_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_policy_snapshot_t.agent_def_id IS 'Identifier for the related agent def.';


--
-- Name: COLUMN agent_policy_snapshot_t.agent_definition_version; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_policy_snapshot_t.agent_definition_version IS 'Pinned Agent definition version represented by this evidence snapshot.';


--
-- Name: COLUMN agent_policy_snapshot_t.agent_publication_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_policy_snapshot_t.agent_publication_id IS 'Pinned Agent publication represented by this evidence snapshot.';


--
-- Name: COLUMN agent_policy_snapshot_t.agent_content_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_policy_snapshot_t.agent_content_digest IS 'Digest of the accepted complete Agent publication.';


--
-- Name: COLUMN agent_policy_snapshot_t.definition_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_policy_snapshot_t.definition_digest IS 'Integrity digest for definition.';


--
-- Name: COLUMN agent_policy_snapshot_t.product_profile_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_policy_snapshot_t.product_profile_digest IS 'Integrity digest for product profile.';


--
-- Name: COLUMN agent_policy_snapshot_t.model_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_policy_snapshot_t.model_digest IS 'Integrity digest for model.';


--
-- Name: COLUMN agent_policy_snapshot_t.catalog_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_policy_snapshot_t.catalog_digest IS 'Integrity digest for catalog.';


--
-- Name: COLUMN agent_policy_snapshot_t.memory_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_policy_snapshot_t.memory_digest IS 'Integrity digest for memory.';


--
-- Name: COLUMN agent_policy_snapshot_t.execution_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_policy_snapshot_t.execution_digest IS 'Integrity digest for execution.';


--
-- Name: COLUMN agent_policy_snapshot_t.channel_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_policy_snapshot_t.channel_digest IS 'Integrity digest for channel.';


--
-- Name: COLUMN agent_policy_snapshot_t.data_boundary_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_policy_snapshot_t.data_boundary_digest IS 'Integrity digest for data boundary.';


--
-- Name: COLUMN agent_policy_snapshot_t.resolved_snapshot; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_policy_snapshot_t.resolved_snapshot IS 'Resolved Snapshot value for this agent policy snapshot record.';


--
-- Name: COLUMN agent_policy_snapshot_t.policy_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_policy_snapshot_t.policy_digest IS 'Integrity digest for policy.';


--
-- Name: COLUMN agent_policy_snapshot_t.revoked_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_policy_snapshot_t.revoked_ts IS 'Timestamp for the revoked event or state.';


--
-- Name: COLUMN agent_policy_snapshot_t.created_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_policy_snapshot_t.created_ts IS 'Timestamp for the created event or state.';


--
-- Name: agent_quota_reservation_t; Type: TABLE; Schema: agent_ops; Owner: -
--

CREATE TABLE agent_ops.agent_quota_reservation_t (
    host_id uuid NOT NULL,
    quota_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    window_start_ts timestamp with time zone NOT NULL,
    reserved_tokens bigint DEFAULT 0 NOT NULL,
    reserved_cost_micros bigint DEFAULT 0 NOT NULL,
    actual_tokens bigint,
    actual_cost_micros bigint,
    accounting_source character varying(32),
    usage_evidence_digest character varying(71),
    reconciled_ts timestamp with time zone,
    created_ts timestamp with time zone DEFAULT now() NOT NULL,
    updated_ts timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT agent_quota_reservation_accounting_source_ck CHECK (((accounting_source IS NULL) OR ((accounting_source)::text = ANY (ARRAY[('trusted-provider'::character varying)::text, ('runner-broker'::character varying)::text, ('reservation-ceiling'::character varying)::text, ('released-no-effect'::character varying)::text, ('legacy-unverified'::character varying)::text])))),
    CONSTRAINT agent_quota_reservation_reconciliation_evidence_ck CHECK ((((reconciled_ts IS NULL) AND (accounting_source IS NULL) AND (usage_evidence_digest IS NULL)) OR ((reconciled_ts IS NOT NULL) AND (accounting_source IS NOT NULL))))
);


--
-- Name: TABLE agent_quota_reservation_t; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON TABLE agent_ops.agent_quota_reservation_t IS 'Stores agent quota reservation records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN agent_quota_reservation_t.host_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_quota_reservation_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN agent_quota_reservation_t.quota_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_quota_reservation_t.quota_id IS 'Identifier for the related quota.';


--
-- Name: COLUMN agent_quota_reservation_t.turn_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_quota_reservation_t.turn_id IS 'Identifier for the related turn.';


--
-- Name: COLUMN agent_quota_reservation_t.window_start_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_quota_reservation_t.window_start_ts IS 'Timestamp for the window start event or state.';


--
-- Name: COLUMN agent_quota_reservation_t.reserved_tokens; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_quota_reservation_t.reserved_tokens IS 'Reserved Tokens value for this agent quota reservation record.';


--
-- Name: COLUMN agent_quota_reservation_t.reserved_cost_micros; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_quota_reservation_t.reserved_cost_micros IS 'Reserved Cost Micros value for this agent quota reservation record.';


--
-- Name: COLUMN agent_quota_reservation_t.actual_tokens; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_quota_reservation_t.actual_tokens IS 'Actual Tokens value for this agent quota reservation record.';


--
-- Name: COLUMN agent_quota_reservation_t.actual_cost_micros; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_quota_reservation_t.actual_cost_micros IS 'Actual Cost Micros value for this agent quota reservation record.';


--
-- Name: COLUMN agent_quota_reservation_t.accounting_source; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_quota_reservation_t.accounting_source IS 'Accounting Source value for this agent quota reservation record.';


--
-- Name: COLUMN agent_quota_reservation_t.usage_evidence_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_quota_reservation_t.usage_evidence_digest IS 'Integrity digest for usage evidence.';


--
-- Name: COLUMN agent_quota_reservation_t.reconciled_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_quota_reservation_t.reconciled_ts IS 'Timestamp for the reconciled event or state.';


--
-- Name: COLUMN agent_quota_reservation_t.created_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_quota_reservation_t.created_ts IS 'Timestamp for the created event or state.';


--
-- Name: COLUMN agent_quota_reservation_t.updated_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_quota_reservation_t.updated_ts IS 'Timestamp for the updated event or state.';


--
-- Name: agent_quota_usage_t; Type: TABLE; Schema: agent_ops; Owner: -
--

CREATE TABLE agent_ops.agent_quota_usage_t (
    host_id uuid NOT NULL,
    quota_id uuid NOT NULL,
    window_start_ts timestamp with time zone NOT NULL,
    quota_policy_version bigint,
    quota_policy_digest character varying(128),
    reserved_tokens bigint DEFAULT 0 NOT NULL,
    reserved_cost_micros bigint DEFAULT 0 NOT NULL,
    consumed_tokens bigint DEFAULT 0 NOT NULL,
    consumed_cost_micros bigint DEFAULT 0 NOT NULL,
    updated_ts timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: TABLE agent_quota_usage_t; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON TABLE agent_ops.agent_quota_usage_t IS 'Stores agent quota usage records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN agent_quota_usage_t.host_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_quota_usage_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN agent_quota_usage_t.quota_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_quota_usage_t.quota_id IS 'Identifier for the related quota.';


--
-- Name: COLUMN agent_quota_usage_t.window_start_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_quota_usage_t.window_start_ts IS 'Timestamp for the window start event or state.';


--
-- Name: COLUMN agent_quota_usage_t.quota_policy_version; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_quota_usage_t.quota_policy_version IS 'Pinned quota-policy version used for this accounting window.';


--
-- Name: COLUMN agent_quota_usage_t.quota_policy_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_quota_usage_t.quota_policy_digest IS 'Pinned quota-policy digest used for this accounting window.';


--
-- Name: COLUMN agent_quota_usage_t.reserved_tokens; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_quota_usage_t.reserved_tokens IS 'Reserved Tokens value for this agent quota usage record.';


--
-- Name: COLUMN agent_quota_usage_t.reserved_cost_micros; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_quota_usage_t.reserved_cost_micros IS 'Reserved Cost Micros value for this agent quota usage record.';


--
-- Name: COLUMN agent_quota_usage_t.consumed_tokens; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_quota_usage_t.consumed_tokens IS 'Consumed Tokens value for this agent quota usage record.';


--
-- Name: COLUMN agent_quota_usage_t.consumed_cost_micros; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_quota_usage_t.consumed_cost_micros IS 'Consumed Cost Micros value for this agent quota usage record.';


--
-- Name: COLUMN agent_quota_usage_t.updated_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_quota_usage_t.updated_ts IS 'Timestamp for the updated event or state.';


--
-- Name: agent_session_event_t; Type: TABLE; Schema: agent_ops; Owner: -
--

CREATE TABLE agent_ops.agent_session_event_t (
    host_id uuid NOT NULL,
    event_id uuid NOT NULL,
    session_id uuid NOT NULL,
    event_sequence bigint NOT NULL,
    turn_id uuid,
    action_attempt_id uuid,
    actor_class character varying(32) NOT NULL,
    event_type character varying(64) NOT NULL,
    content jsonb NOT NULL,
    content_digest character varying(71) NOT NULL,
    policy_digest character varying(71) NOT NULL,
    visibility character varying(16) DEFAULT 'USER'::character varying NOT NULL,
    retention_class character varying(32) DEFAULT 'STANDARD'::character varying NOT NULL,
    created_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    CONSTRAINT agent_session_event_t_event_sequence_check CHECK ((event_sequence > 0))
);


--
-- Name: TABLE agent_session_event_t; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON TABLE agent_ops.agent_session_event_t IS 'Stores agent session event records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN agent_session_event_t.host_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_event_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN agent_session_event_t.event_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_event_t.event_id IS 'Identifier for the related event.';


--
-- Name: COLUMN agent_session_event_t.session_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_event_t.session_id IS 'Identifier for the related session.';


--
-- Name: COLUMN agent_session_event_t.event_sequence; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_event_t.event_sequence IS 'Event Sequence value for this agent session event record.';


--
-- Name: COLUMN agent_session_event_t.turn_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_event_t.turn_id IS 'Identifier for the related turn.';


--
-- Name: COLUMN agent_session_event_t.action_attempt_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_event_t.action_attempt_id IS 'Identifier for the related action attempt.';


--
-- Name: COLUMN agent_session_event_t.actor_class; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_event_t.actor_class IS 'Actor Class value for this agent session event record.';


--
-- Name: COLUMN agent_session_event_t.event_type; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_event_t.event_type IS 'Event Type value for this agent session event record.';


--
-- Name: COLUMN agent_session_event_t.content; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_event_t.content IS 'Content value for this agent session event record.';


--
-- Name: COLUMN agent_session_event_t.content_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_event_t.content_digest IS 'Integrity digest for content.';


--
-- Name: COLUMN agent_session_event_t.policy_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_event_t.policy_digest IS 'Integrity digest for policy.';


--
-- Name: COLUMN agent_session_event_t.visibility; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_event_t.visibility IS 'Visibility value for this agent session event record.';


--
-- Name: COLUMN agent_session_event_t.retention_class; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_event_t.retention_class IS 'Retention Class value for this agent session event record.';


--
-- Name: COLUMN agent_session_event_t.created_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_event_t.created_ts IS 'Timestamp for the created event or state.';


--
-- Name: agent_session_history_t; Type: TABLE; Schema: agent_ops; Owner: -
--

CREATE TABLE agent_ops.agent_session_history_t (
    host_id uuid NOT NULL,
    session_id uuid NOT NULL,
    bank_id uuid NOT NULL,
    messages jsonb DEFAULT '[]'::jsonb NOT NULL,
    metadata jsonb DEFAULT '{}'::jsonb,
    aggregate_version bigint DEFAULT 1 NOT NULL,
    active boolean DEFAULT true,
    update_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP,
    update_user character varying(126) DEFAULT SESSION_USER,
    durable_session_id uuid,
    projection_sequence bigint DEFAULT 0 NOT NULL,
    projection_state character varying(16) DEFAULT 'CURRENT'::character varying NOT NULL
);


--
-- Name: TABLE agent_session_history_t; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON TABLE agent_ops.agent_session_history_t IS 'Stores agent session history records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN agent_session_history_t.host_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_history_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN agent_session_history_t.session_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_history_t.session_id IS 'Identifier for the related session.';


--
-- Name: COLUMN agent_session_history_t.bank_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_history_t.bank_id IS 'Identifier for the related bank.';


--
-- Name: COLUMN agent_session_history_t.messages; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_history_t.messages IS 'Messages value for this agent session history record.';


--
-- Name: COLUMN agent_session_history_t.metadata; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_history_t.metadata IS 'Metadata value for this agent session history record.';


--
-- Name: COLUMN agent_session_history_t.aggregate_version; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_history_t.aggregate_version IS 'Version value for aggregate.';


--
-- Name: COLUMN agent_session_history_t.active; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_history_t.active IS 'Indicates whether this record is active.';


--
-- Name: COLUMN agent_session_history_t.update_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_history_t.update_ts IS 'Timestamp when this record was last updated.';


--
-- Name: COLUMN agent_session_history_t.update_user; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_history_t.update_user IS 'User or service principal that last updated this record.';


--
-- Name: COLUMN agent_session_history_t.durable_session_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_history_t.durable_session_id IS 'Identifier for the related durable session.';


--
-- Name: COLUMN agent_session_history_t.projection_sequence; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_history_t.projection_sequence IS 'Projection Sequence value for this agent session history record.';


--
-- Name: COLUMN agent_session_history_t.projection_state; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_history_t.projection_state IS 'Projection State value for this agent session history record.';


--
-- Name: agent_session_t; Type: TABLE; Schema: agent_ops; Owner: -
--

CREATE TABLE agent_ops.agent_session_t (
    host_id uuid NOT NULL,
    session_id uuid NOT NULL,
    principal_id character varying(255) NOT NULL,
    user_id uuid,
    agent_def_id uuid NOT NULL,
    agent_definition_version bigint NOT NULL,
    bank_id uuid,
    execution_session_id uuid,
    policy_snapshot_id uuid NOT NULL,
    state character varying(16) DEFAULT 'ACTIVE'::character varying NOT NULL,
    session_version bigint DEFAULT 1 NOT NULL,
    active_turn_id uuid,
    next_turn_sequence bigint DEFAULT 1 NOT NULL,
    next_queue_sequence bigint DEFAULT 1 NOT NULL,
    idle_expires_ts timestamp with time zone NOT NULL,
    maximum_expires_ts timestamp with time zone NOT NULL,
    resume_handle_digest character varying(71) NOT NULL,
    resume_revoked_ts timestamp with time zone,
    approval_hold_id uuid,
    approval_hold_state character varying(32),
    approval_hold_expires_ts timestamp with time zone,
    preserved_workspace_ref character varying(1024),
    cleanup_state character varying(32) DEFAULT 'NOT_REQUIRED'::character varying NOT NULL,
    cleanup_request_id uuid,
    cleanup_evidence jsonb,
    created_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    workspace_compatibility jsonb,
    workspace_compatibility_digest character varying(71),
    workspace_checkpoint_digest character varying(71),
    workspace_effective_expires_ts timestamp with time zone,
    service_pool_id uuid,
    service_pool_compatibility_digest character varying(71),
    service_pool_maximum_concurrency integer,
    agent_publication_id uuid,
    agent_content_digest character varying(128),
    agent_definition_digest character varying(128),
    user_identity_digest character varying(128),
    model_provider character varying(64),
    model_name character varying(126),
    execution_session_reference_digest character varying(128),
    CONSTRAINT agent_session_t_check CHECK ((idle_expires_ts <= maximum_expires_ts)),
    CONSTRAINT agent_session_t_check1 CHECK ((((approval_hold_id IS NULL) AND (approval_hold_state IS NULL) AND (approval_hold_expires_ts IS NULL)) OR ((approval_hold_id IS NOT NULL) AND (approval_hold_state IS NOT NULL) AND (approval_hold_expires_ts IS NOT NULL)))),
    CONSTRAINT agent_session_t_cleanup_state_check CHECK (((cleanup_state)::text = ANY (ARRAY[('NOT_REQUIRED'::character varying)::text, ('CLEANUP_REQUESTED'::character varying)::text, ('CLEANUP_PENDING'::character varying)::text, ('CLEANED'::character varying)::text, ('CLEANUP_FAILED'::character varying)::text]))),
    CONSTRAINT agent_session_t_next_queue_sequence_check CHECK ((next_queue_sequence > 0)),
    CONSTRAINT agent_session_t_next_turn_sequence_check CHECK ((next_turn_sequence > 0)),
    CONSTRAINT agent_session_t_session_version_check CHECK ((session_version > 0)),
    CONSTRAINT agent_session_t_state_check CHECK (((state)::text = ANY (ARRAY[('ACTIVE'::character varying)::text, ('CLOSING'::character varying)::text, ('CLOSED'::character varying)::text, ('REVOKED'::character varying)::text, ('EXPIRED'::character varying)::text])))
);


--
-- Name: TABLE agent_session_t; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON TABLE agent_ops.agent_session_t IS 'Stores agent session records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN agent_session_t.host_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN agent_session_t.session_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.session_id IS 'Identifier for the related session.';


--
-- Name: COLUMN agent_session_t.principal_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.principal_id IS 'Identifier for the related principal.';


--
-- Name: COLUMN agent_session_t.user_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.user_id IS 'Identifier for the related user.';


--
-- Name: COLUMN agent_session_t.agent_def_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.agent_def_id IS 'Identifier for the related agent def.';


--
-- Name: COLUMN agent_session_t.agent_definition_version; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.agent_definition_version IS 'Version value for agent definition.';


--
-- Name: COLUMN agent_session_t.bank_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.bank_id IS 'Identifier for the related bank.';


--
-- Name: COLUMN agent_session_t.execution_session_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.execution_session_id IS 'Identifier for the related execution session.';


--
-- Name: COLUMN agent_session_t.policy_snapshot_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.policy_snapshot_id IS 'Identifier for the related policy snapshot.';


--
-- Name: COLUMN agent_session_t.state; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.state IS 'State value for this agent session record.';


--
-- Name: COLUMN agent_session_t.session_version; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.session_version IS 'Version value for session.';


--
-- Name: COLUMN agent_session_t.active_turn_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.active_turn_id IS 'Identifier for the related active turn.';


--
-- Name: COLUMN agent_session_t.next_turn_sequence; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.next_turn_sequence IS 'Next Turn Sequence value for this agent session record.';


--
-- Name: COLUMN agent_session_t.next_queue_sequence; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.next_queue_sequence IS 'Next Queue Sequence value for this agent session record.';


--
-- Name: COLUMN agent_session_t.idle_expires_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.idle_expires_ts IS 'Timestamp for the idle expires event or state.';


--
-- Name: COLUMN agent_session_t.maximum_expires_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.maximum_expires_ts IS 'Timestamp for the maximum expires event or state.';


--
-- Name: COLUMN agent_session_t.resume_handle_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.resume_handle_digest IS 'Integrity digest for resume handle.';


--
-- Name: COLUMN agent_session_t.resume_revoked_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.resume_revoked_ts IS 'Timestamp for the resume revoked event or state.';


--
-- Name: COLUMN agent_session_t.approval_hold_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.approval_hold_id IS 'Identifier for the related approval hold.';


--
-- Name: COLUMN agent_session_t.approval_hold_state; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.approval_hold_state IS 'Approval Hold State value for this agent session record.';


--
-- Name: COLUMN agent_session_t.approval_hold_expires_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.approval_hold_expires_ts IS 'Timestamp for the approval hold expires event or state.';


--
-- Name: COLUMN agent_session_t.preserved_workspace_ref; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.preserved_workspace_ref IS 'Preserved Workspace Ref value for this agent session record.';


--
-- Name: COLUMN agent_session_t.cleanup_state; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.cleanup_state IS 'Cleanup State value for this agent session record.';


--
-- Name: COLUMN agent_session_t.cleanup_request_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.cleanup_request_id IS 'Identifier for the related cleanup request.';


--
-- Name: COLUMN agent_session_t.cleanup_evidence; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.cleanup_evidence IS 'Cleanup Evidence value for this agent session record.';


--
-- Name: COLUMN agent_session_t.created_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.created_ts IS 'Timestamp for the created event or state.';


--
-- Name: COLUMN agent_session_t.updated_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.updated_ts IS 'Timestamp for the updated event or state.';


--
-- Name: COLUMN agent_session_t.workspace_compatibility; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.workspace_compatibility IS 'Workspace Compatibility value for this agent session record.';


--
-- Name: COLUMN agent_session_t.workspace_compatibility_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.workspace_compatibility_digest IS 'Integrity digest for workspace compatibility.';


--
-- Name: COLUMN agent_session_t.workspace_checkpoint_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.workspace_checkpoint_digest IS 'Integrity digest for workspace checkpoint.';


--
-- Name: COLUMN agent_session_t.workspace_effective_expires_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.workspace_effective_expires_ts IS 'Timestamp for the workspace effective expires event or state.';


--
-- Name: COLUMN agent_session_t.service_pool_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.service_pool_id IS 'Identifier for the related service pool.';


--
-- Name: COLUMN agent_session_t.service_pool_compatibility_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.service_pool_compatibility_digest IS 'Integrity digest for service pool compatibility.';


--
-- Name: COLUMN agent_session_t.service_pool_maximum_concurrency; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.service_pool_maximum_concurrency IS 'Pinned service-pool capacity used only with operational occupancy rows.';


--
-- Name: COLUMN agent_session_t.agent_publication_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.agent_publication_id IS 'Pinned Agent publication accepted at session admission.';


--
-- Name: COLUMN agent_session_t.agent_content_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.agent_content_digest IS 'Digest of the complete Agent publication accepted at session admission.';


--
-- Name: COLUMN agent_session_t.agent_definition_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.agent_definition_digest IS 'Pinned Agent definition digest accepted at session admission.';


--
-- Name: COLUMN agent_session_t.user_identity_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.user_identity_digest IS 'Digest of the authenticated user identity accepted at session admission.';


--
-- Name: COLUMN agent_session_t.model_provider; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.model_provider IS 'Pinned model provider accepted at session admission.';


--
-- Name: COLUMN agent_session_t.model_name; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.model_name IS 'Pinned model alias accepted at session admission.';


--
-- Name: COLUMN agent_session_t.execution_session_reference_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_session_t.execution_session_reference_digest IS 'Digest of the authenticated execution-session reference.';


--
-- Name: agent_turn_materialization_t; Type: TABLE; Schema: agent_ops; Owner: -
--

CREATE TABLE agent_ops.agent_turn_materialization_t (
    host_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    materializer_id character varying(126) NOT NULL,
    materializer_version integer NOT NULL,
    product_profile character varying(64) NOT NULL,
    manifest jsonb NOT NULL,
    manifest_digest character varying(71) NOT NULL,
    created_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL
);


--
-- Name: TABLE agent_turn_materialization_t; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON TABLE agent_ops.agent_turn_materialization_t IS 'Stores agent turn materialization records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN agent_turn_materialization_t.host_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_materialization_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN agent_turn_materialization_t.turn_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_materialization_t.turn_id IS 'Identifier for the related turn.';


--
-- Name: COLUMN agent_turn_materialization_t.materializer_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_materialization_t.materializer_id IS 'Identifier for the related materializer.';


--
-- Name: COLUMN agent_turn_materialization_t.materializer_version; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_materialization_t.materializer_version IS 'Version value for materializer.';


--
-- Name: COLUMN agent_turn_materialization_t.product_profile; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_materialization_t.product_profile IS 'Product Profile value for this agent turn materialization record.';


--
-- Name: COLUMN agent_turn_materialization_t.manifest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_materialization_t.manifest IS 'Manifest value for this agent turn materialization record.';


--
-- Name: COLUMN agent_turn_materialization_t.manifest_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_materialization_t.manifest_digest IS 'Integrity digest for manifest.';


--
-- Name: COLUMN agent_turn_materialization_t.created_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_materialization_t.created_ts IS 'Timestamp for the created event or state.';


--
-- Name: agent_turn_t; Type: TABLE; Schema: agent_ops; Owner: -
--

CREATE TABLE agent_ops.agent_turn_t (
    host_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    session_id uuid NOT NULL,
    turn_sequence bigint NOT NULL,
    queue_sequence bigint NOT NULL,
    origin_kind character varying(16) NOT NULL,
    origin_ref character varying(255),
    client_message_id character varying(255) NOT NULL,
    idempotency_key character varying(255) NOT NULL,
    state character varying(32) DEFAULT 'QUEUED'::character varying NOT NULL,
    policy_snapshot_id uuid NOT NULL,
    policy_digest character varying(71) NOT NULL,
    data_boundary_digest character varying(71) NOT NULL,
    model_provider character varying(64) NOT NULL,
    model_name character varying(126) NOT NULL,
    model_action_budget integer NOT NULL,
    token_budget bigint NOT NULL,
    cost_budget_micros bigint NOT NULL,
    deadline_ts timestamp with time zone NOT NULL,
    delegation_depth integer DEFAULT 0 NOT NULL,
    terminal_result jsonb,
    terminal_error jsonb,
    event_sequence bigint DEFAULT 0 NOT NULL,
    projection_sequence bigint DEFAULT 0 NOT NULL,
    activated_ts timestamp with time zone,
    terminal_ts timestamp with time zone,
    created_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    scheduling_request_id uuid,
    execution_attempt_id uuid,
    scheduling_request_reference_digest character varying(128),
    execution_reference_digest character varying(128),
    materialization_manifest_digest character varying(71),
    coding_base_revision character varying(64),
    coding_patch_digest character varying(71),
    quota_input_cost_micros_per_million bigint DEFAULT 0 NOT NULL,
    quota_output_cost_micros_per_million bigint DEFAULT 0 NOT NULL,
    service_pool_id uuid,
    CONSTRAINT agent_turn_quota_rates_nonnegative_ck CHECK (((quota_input_cost_micros_per_million >= 0) AND (quota_output_cost_micros_per_million >= 0))),
    CONSTRAINT agent_turn_t_cost_budget_micros_check CHECK ((cost_budget_micros >= 0)),
    CONSTRAINT agent_turn_t_delegation_depth_check CHECK ((delegation_depth >= 0)),
    CONSTRAINT agent_turn_t_model_action_budget_check CHECK ((model_action_budget > 0)),
    CONSTRAINT agent_turn_t_origin_kind_check CHECK (((origin_kind)::text = ANY (ARRAY[('user'::character varying)::text, ('channel'::character varying)::text, ('workflow'::character varying)::text, ('scheduler'::character varying)::text, ('connector'::character varying)::text, ('service'::character varying)::text]))),
    CONSTRAINT agent_turn_t_queue_sequence_check CHECK ((queue_sequence > 0)),
    CONSTRAINT agent_turn_t_state_check CHECK (((state)::text = ANY (ARRAY[('QUEUED'::character varying)::text, ('RECEIVED'::character varying)::text, ('RUNNING_MODEL'::character varying)::text, ('WAITING_ACTION'::character varying)::text, ('RUNNING_ACTION'::character varying)::text, ('WAITING_RECONCILIATION'::character varying)::text, ('WAITING_APPROVAL'::character varying)::text, ('COMPLETED'::character varying)::text, ('FAILED'::character varying)::text, ('CANCELLED'::character varying)::text, ('UNKNOWN'::character varying)::text]))),
    CONSTRAINT agent_turn_t_token_budget_check CHECK ((token_budget > 0)),
    CONSTRAINT agent_turn_t_turn_sequence_check CHECK ((turn_sequence > 0))
);


--
-- Name: TABLE agent_turn_t; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON TABLE agent_ops.agent_turn_t IS 'Stores agent turn records used by the Light Workflow, Light Agent, and execution runtime services.';


--
-- Name: COLUMN agent_turn_t.host_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.host_id IS 'Tenant host identifier that scopes this record.';


--
-- Name: COLUMN agent_turn_t.turn_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.turn_id IS 'Identifier for the related turn.';


--
-- Name: COLUMN agent_turn_t.session_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.session_id IS 'Identifier for the related session.';


--
-- Name: COLUMN agent_turn_t.turn_sequence; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.turn_sequence IS 'Turn Sequence value for this agent turn record.';


--
-- Name: COLUMN agent_turn_t.queue_sequence; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.queue_sequence IS 'Queue Sequence value for this agent turn record.';


--
-- Name: COLUMN agent_turn_t.origin_kind; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.origin_kind IS 'Origin Kind value for this agent turn record.';


--
-- Name: COLUMN agent_turn_t.origin_ref; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.origin_ref IS 'Origin Ref value for this agent turn record.';


--
-- Name: COLUMN agent_turn_t.client_message_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.client_message_id IS 'Identifier for the related client message.';


--
-- Name: COLUMN agent_turn_t.idempotency_key; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.idempotency_key IS 'Idempotency Key value for this agent turn record.';


--
-- Name: COLUMN agent_turn_t.state; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.state IS 'State value for this agent turn record.';


--
-- Name: COLUMN agent_turn_t.policy_snapshot_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.policy_snapshot_id IS 'Identifier for the related policy snapshot.';


--
-- Name: COLUMN agent_turn_t.policy_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.policy_digest IS 'Integrity digest for policy.';


--
-- Name: COLUMN agent_turn_t.data_boundary_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.data_boundary_digest IS 'Integrity digest for data boundary.';


--
-- Name: COLUMN agent_turn_t.model_provider; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.model_provider IS 'Model Provider value for this agent turn record.';


--
-- Name: COLUMN agent_turn_t.model_name; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.model_name IS 'Model Name value for this agent turn record.';


--
-- Name: COLUMN agent_turn_t.model_action_budget; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.model_action_budget IS 'Model Action Budget value for this agent turn record.';


--
-- Name: COLUMN agent_turn_t.token_budget; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.token_budget IS 'Token Budget value for this agent turn record.';


--
-- Name: COLUMN agent_turn_t.cost_budget_micros; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.cost_budget_micros IS 'Cost Budget Micros value for this agent turn record.';


--
-- Name: COLUMN agent_turn_t.deadline_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.deadline_ts IS 'Timestamp for the deadline event or state.';


--
-- Name: COLUMN agent_turn_t.delegation_depth; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.delegation_depth IS 'Delegation Depth value for this agent turn record.';


--
-- Name: COLUMN agent_turn_t.terminal_result; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.terminal_result IS 'Terminal Result value for this agent turn record.';


--
-- Name: COLUMN agent_turn_t.terminal_error; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.terminal_error IS 'Terminal Error value for this agent turn record.';


--
-- Name: COLUMN agent_turn_t.event_sequence; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.event_sequence IS 'Event Sequence value for this agent turn record.';


--
-- Name: COLUMN agent_turn_t.projection_sequence; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.projection_sequence IS 'Projection Sequence value for this agent turn record.';


--
-- Name: COLUMN agent_turn_t.activated_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.activated_ts IS 'Timestamp for the activated event or state.';


--
-- Name: COLUMN agent_turn_t.terminal_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.terminal_ts IS 'Timestamp for the terminal event or state.';


--
-- Name: COLUMN agent_turn_t.created_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.created_ts IS 'Timestamp for the created event or state.';


--
-- Name: COLUMN agent_turn_t.updated_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.updated_ts IS 'Timestamp for the updated event or state.';


--
-- Name: COLUMN agent_turn_t.scheduling_request_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.scheduling_request_id IS 'Identifier for the related scheduling request.';


--
-- Name: COLUMN agent_turn_t.execution_attempt_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.execution_attempt_id IS 'Identifier for the related execution attempt.';


--
-- Name: COLUMN agent_turn_t.scheduling_request_reference_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.scheduling_request_reference_digest IS 'Digest of the authenticated scheduling-request reference.';


--
-- Name: COLUMN agent_turn_t.execution_reference_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.execution_reference_digest IS 'Digest of the authenticated execution-attempt reference.';


--
-- Name: COLUMN agent_turn_t.materialization_manifest_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.materialization_manifest_digest IS 'Integrity digest for materialization manifest.';


--
-- Name: COLUMN agent_turn_t.coding_base_revision; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.coding_base_revision IS 'Coding Base Revision value for this agent turn record.';


--
-- Name: COLUMN agent_turn_t.coding_patch_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.coding_patch_digest IS 'Integrity digest for coding patch.';


--
-- Name: COLUMN agent_turn_t.quota_input_cost_micros_per_million; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.quota_input_cost_micros_per_million IS 'Quota Input Cost Micros Per Million value for this agent turn record.';


--
-- Name: COLUMN agent_turn_t.quota_output_cost_micros_per_million; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.quota_output_cost_micros_per_million IS 'Quota Output Cost Micros Per Million value for this agent turn record.';


--
-- Name: COLUMN agent_turn_t.service_pool_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.agent_turn_t.service_pool_id IS 'Identifier for the related service pool.';


--
-- Name: operational_reference_evidence_t; Type: TABLE; Schema: agent_ops; Owner: -
--

CREATE TABLE agent_ops.operational_reference_evidence_t (
    host_id uuid NOT NULL,
    reference_id uuid NOT NULL,
    source_service character varying(255) NOT NULL,
    source_table character varying(126) NOT NULL,
    source_record_id uuid NOT NULL,
    reference_kind character varying(64) NOT NULL,
    target_id uuid NOT NULL,
    target_version bigint,
    publication_id uuid,
    content_digest character varying(128) NOT NULL,
    issuer character varying(255) NOT NULL,
    audience character varying(64) NOT NULL,
    state character varying(16) DEFAULT 'ACCEPTED'::character varying NOT NULL,
    evidence jsonb DEFAULT '{}'::jsonb NOT NULL,
    accepted_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    reconciled_ts timestamp with time zone,
    CONSTRAINT operational_reference_evidence_digest_ck CHECK (((content_digest)::text ~ '^(sha256:)?[0-9A-Fa-f]{64}$'::text)),
    CONSTRAINT operational_reference_evidence_state_ck CHECK (((state)::text = ANY ((ARRAY['ACCEPTED'::character varying, 'MISSING'::character varying, 'STALE'::character varying, 'REVOKED'::character varying])::text[]))),
    CONSTRAINT operational_reference_evidence_version_ck CHECK (((target_version IS NULL) OR (target_version > 0)))
);


--
-- Name: TABLE operational_reference_evidence_t; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON TABLE agent_ops.operational_reference_evidence_t IS 'Pinned application-level evidence replacing control-plane and cross-service foreign keys.';


--
-- Name: COLUMN operational_reference_evidence_t.host_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_evidence_t.host_id IS 'Tenant Host scope accepted by the runtime projection.';


--
-- Name: COLUMN operational_reference_evidence_t.reference_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_evidence_t.reference_id IS 'Stable identifier for this accepted reference evidence.';


--
-- Name: COLUMN operational_reference_evidence_t.source_service; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_evidence_t.source_service IS 'Runtime service that admitted the source record.';


--
-- Name: COLUMN operational_reference_evidence_t.source_table; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_evidence_t.source_table IS 'Logical source table owning the reference.';


--
-- Name: COLUMN operational_reference_evidence_t.source_record_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_evidence_t.source_record_id IS 'Stable identifier of the source operational record.';


--
-- Name: COLUMN operational_reference_evidence_t.reference_kind; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_evidence_t.reference_kind IS 'Stable application-level reference kind.';


--
-- Name: COLUMN operational_reference_evidence_t.target_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_evidence_t.target_id IS 'Pinned target identifier.';


--
-- Name: COLUMN operational_reference_evidence_t.target_version; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_evidence_t.target_version IS 'Pinned target version when the target is versioned.';


--
-- Name: COLUMN operational_reference_evidence_t.publication_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_evidence_t.publication_id IS 'Accepted control-plane publication identifier when applicable.';


--
-- Name: COLUMN operational_reference_evidence_t.content_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_evidence_t.content_digest IS 'Accepted target or publication content digest.';


--
-- Name: COLUMN operational_reference_evidence_t.issuer; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_evidence_t.issuer IS 'Authenticated service or projection issuer.';


--
-- Name: COLUMN operational_reference_evidence_t.audience; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_evidence_t.audience IS 'Runtime audience authorized to consume the reference.';


--
-- Name: COLUMN operational_reference_evidence_t.state; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_evidence_t.state IS 'Current reconciliation state of the accepted reference.';


--
-- Name: COLUMN operational_reference_evidence_t.evidence; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_evidence_t.evidence IS 'Bounded non-secret admission evidence.';


--
-- Name: COLUMN operational_reference_evidence_t.accepted_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_evidence_t.accepted_ts IS 'Timestamp when the application accepted the reference.';


--
-- Name: COLUMN operational_reference_evidence_t.reconciled_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_evidence_t.reconciled_ts IS 'Timestamp of the most recent reconciliation.';


--
-- Name: operational_reference_reconciliation_t; Type: TABLE; Schema: agent_ops; Owner: -
--

CREATE TABLE agent_ops.operational_reference_reconciliation_t (
    host_id uuid NOT NULL,
    reconciliation_id uuid NOT NULL,
    reference_id uuid NOT NULL,
    source_service character varying(255) NOT NULL,
    source_table character varying(126) NOT NULL,
    source_record_id uuid NOT NULL,
    reference_kind character varying(64) NOT NULL,
    target_id uuid NOT NULL,
    accepted_digest character varying(128) NOT NULL,
    observed_digest character varying(128),
    status character varying(16) NOT NULL,
    diagnostic jsonb DEFAULT '{}'::jsonb NOT NULL,
    checked_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    CONSTRAINT operational_reference_reconciliation_digest_ck CHECK ((((accepted_digest)::text ~ '^(sha256:)?[0-9A-Fa-f]{64}$'::text) AND ((observed_digest IS NULL) OR ((observed_digest)::text ~ '^(sha256:)?[0-9A-Fa-f]{64}$'::text)))),
    CONSTRAINT operational_reference_reconciliation_status_ck CHECK (((status)::text = ANY ((ARRAY['CURRENT'::character varying, 'MISSING'::character varying, 'STALE'::character varying, 'REVOKED'::character varying])::text[])))
);


--
-- Name: TABLE operational_reference_reconciliation_t; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON TABLE agent_ops.operational_reference_reconciliation_t IS 'Append-only reconciliation outcomes for pinned operational references.';


--
-- Name: COLUMN operational_reference_reconciliation_t.host_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_reconciliation_t.host_id IS 'Tenant Host scope for the reconciliation.';


--
-- Name: COLUMN operational_reference_reconciliation_t.reconciliation_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_reconciliation_t.reconciliation_id IS 'Stable identifier for this reconciliation outcome.';


--
-- Name: COLUMN operational_reference_reconciliation_t.reference_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_reconciliation_t.reference_id IS 'Accepted reference identifier being reconciled.';


--
-- Name: COLUMN operational_reference_reconciliation_t.source_service; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_reconciliation_t.source_service IS 'Runtime service owning the source record.';


--
-- Name: COLUMN operational_reference_reconciliation_t.source_table; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_reconciliation_t.source_table IS 'Logical source table owning the reference.';


--
-- Name: COLUMN operational_reference_reconciliation_t.source_record_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_reconciliation_t.source_record_id IS 'Stable identifier of the source operational record.';


--
-- Name: COLUMN operational_reference_reconciliation_t.reference_kind; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_reconciliation_t.reference_kind IS 'Stable application-level reference kind.';


--
-- Name: COLUMN operational_reference_reconciliation_t.target_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_reconciliation_t.target_id IS 'Pinned target identifier.';


--
-- Name: COLUMN operational_reference_reconciliation_t.accepted_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_reconciliation_t.accepted_digest IS 'Digest accepted at admission.';


--
-- Name: COLUMN operational_reference_reconciliation_t.observed_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_reconciliation_t.observed_digest IS 'Digest observed during reconciliation, when present.';


--
-- Name: COLUMN operational_reference_reconciliation_t.status; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_reconciliation_t.status IS 'CURRENT, MISSING, STALE, or REVOKED reconciliation result.';


--
-- Name: COLUMN operational_reference_reconciliation_t.diagnostic; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_reconciliation_t.diagnostic IS 'Bounded non-secret reconciliation diagnostics.';


--
-- Name: COLUMN operational_reference_reconciliation_t.checked_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.operational_reference_reconciliation_t.checked_ts IS 'Timestamp when reconciliation completed.';


--
-- Name: runtime_operational_scope_t; Type: TABLE; Schema: agent_ops; Owner: -
--

CREATE TABLE agent_ops.runtime_operational_scope_t (
    host_id uuid NOT NULL,
    environment character varying(64) NOT NULL,
    service_id character varying(255) NOT NULL,
    instance_id uuid NOT NULL,
    publication_id uuid NOT NULL,
    content_digest character varying(128) NOT NULL,
    audience character varying(64) NOT NULL,
    active boolean DEFAULT true NOT NULL,
    accepted_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    last_seen_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    CONSTRAINT runtime_operational_scope_digest_ck CHECK (((content_digest)::text ~ '^(sha256:)?[0-9A-Fa-f]{64}$'::text))
);


--
-- Name: TABLE runtime_operational_scope_t; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON TABLE agent_ops.runtime_operational_scope_t IS 'Accepted runtime Host and environment scope used without a Config Server host foreign key.';


--
-- Name: COLUMN runtime_operational_scope_t.host_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.runtime_operational_scope_t.host_id IS 'Tenant Host identifier accepted from the runtime projection.';


--
-- Name: COLUMN runtime_operational_scope_t.environment; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.runtime_operational_scope_t.environment IS 'Environment tag accepted from the runtime projection.';


--
-- Name: COLUMN runtime_operational_scope_t.service_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.runtime_operational_scope_t.service_id IS 'Service identifier bound to this runtime scope.';


--
-- Name: COLUMN runtime_operational_scope_t.instance_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.runtime_operational_scope_t.instance_id IS 'Runtime instance identifier bound to this scope.';


--
-- Name: COLUMN runtime_operational_scope_t.publication_id; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.runtime_operational_scope_t.publication_id IS 'Control-plane publication identifier accepted by the runtime.';


--
-- Name: COLUMN runtime_operational_scope_t.content_digest; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.runtime_operational_scope_t.content_digest IS 'Digest of the accepted audience projection.';


--
-- Name: COLUMN runtime_operational_scope_t.audience; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.runtime_operational_scope_t.audience IS 'Projection audience accepted by the runtime.';


--
-- Name: COLUMN runtime_operational_scope_t.active; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.runtime_operational_scope_t.active IS 'Whether the scope remains eligible for admission.';


--
-- Name: COLUMN runtime_operational_scope_t.accepted_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.runtime_operational_scope_t.accepted_ts IS 'Timestamp when the runtime first accepted the scope.';


--
-- Name: COLUMN runtime_operational_scope_t.last_seen_ts; Type: COMMENT; Schema: agent_ops; Owner: -
--

COMMENT ON COLUMN agent_ops.runtime_operational_scope_t.last_seen_ts IS 'Timestamp when the runtime most recently confirmed the scope.';


--
-- Name: agent_action_attempt_t agent_action_attempt_t_host_id_execution_attempt_id_key; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_action_attempt_t
    ADD CONSTRAINT agent_action_attempt_t_host_id_execution_attempt_id_key UNIQUE (host_id, execution_attempt_id);


--
-- Name: agent_action_attempt_t agent_action_attempt_t_host_id_gateway_request_id_key; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_action_attempt_t
    ADD CONSTRAINT agent_action_attempt_t_host_id_gateway_request_id_key UNIQUE (host_id, gateway_request_id);


--
-- Name: agent_action_attempt_t agent_action_attempt_t_host_id_turn_id_logical_action_id_at_key; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_action_attempt_t
    ADD CONSTRAINT agent_action_attempt_t_host_id_turn_id_logical_action_id_at_key UNIQUE (host_id, turn_id, logical_action_id, attempt_number);


--
-- Name: agent_action_attempt_t agent_action_attempt_t_pkey; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_action_attempt_t
    ADD CONSTRAINT agent_action_attempt_t_pkey PRIMARY KEY (host_id, action_attempt_id);


--
-- Name: agent_approval_t agent_approval_t_host_id_nonce_digest_key; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_approval_t
    ADD CONSTRAINT agent_approval_t_host_id_nonce_digest_key UNIQUE (host_id, nonce_digest);


--
-- Name: agent_approval_t agent_approval_t_pkey; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_approval_t
    ADD CONSTRAINT agent_approval_t_pkey PRIMARY KEY (host_id, approval_id);


--
-- Name: agent_delegation_replay_t agent_delegation_replay_t_pkey; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_delegation_replay_t
    ADD CONSTRAINT agent_delegation_replay_t_pkey PRIMARY KEY (audience, replay_id);


--
-- Name: agent_execution_outbox_t agent_execution_outbox_t_pkey; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_execution_outbox_t
    ADD CONSTRAINT agent_execution_outbox_t_pkey PRIMARY KEY (host_id, dispatch_id);


--
-- Name: agent_execution_outbox_t agent_execution_outbox_t_request_key; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_execution_outbox_t
    ADD CONSTRAINT agent_execution_outbox_t_request_key UNIQUE (host_id, request_id, command_kind);


--
-- Name: agent_fixed_action_t agent_fixed_action_t_host_id_approval_nonce_digest_key; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_fixed_action_t
    ADD CONSTRAINT agent_fixed_action_t_host_id_approval_nonce_digest_key UNIQUE (host_id, approval_nonce_digest);


--
-- Name: agent_fixed_action_t agent_fixed_action_t_pkey; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_fixed_action_t
    ADD CONSTRAINT agent_fixed_action_t_pkey PRIMARY KEY (host_id, fixed_action_id);


--
-- Name: agent_job_t agent_job_t_host_id_idempotency_key_key; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_job_t
    ADD CONSTRAINT agent_job_t_host_id_idempotency_key_key UNIQUE (host_id, idempotency_key);


--
-- Name: agent_job_t agent_job_t_pkey; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_job_t
    ADD CONSTRAINT agent_job_t_pkey PRIMARY KEY (host_id, job_id);


--
-- Name: agent_memory_bank_t agent_memory_bank_t_pkey; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_memory_bank_t
    ADD CONSTRAINT agent_memory_bank_t_pkey PRIMARY KEY (host_id, bank_id);


--
-- Name: agent_memory_doc_t agent_memory_doc_t_pkey; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_memory_doc_t
    ADD CONSTRAINT agent_memory_doc_t_pkey PRIMARY KEY (host_id, bank_id, doc_id);


--
-- Name: agent_memory_entity_cooccur_t agent_memory_entity_cooccur_t_pkey; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_memory_entity_cooccur_t
    ADD CONSTRAINT agent_memory_entity_cooccur_t_pkey PRIMARY KEY (host_id, bank_id, entity_id_1, entity_id_2);


--
-- Name: agent_memory_entity_t agent_memory_entity_t_pkey; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_memory_entity_t
    ADD CONSTRAINT agent_memory_entity_t_pkey PRIMARY KEY (host_id, bank_id, entity_id);


--
-- Name: agent_memory_link_t agent_memory_link_t_pkey; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_memory_link_t
    ADD CONSTRAINT agent_memory_link_t_pkey PRIMARY KEY (host_id, bank_id, from_unit_id, to_unit_id, link_type);


--
-- Name: agent_memory_reflection_t agent_memory_reflection_t_pkey; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_memory_reflection_t
    ADD CONSTRAINT agent_memory_reflection_t_pkey PRIMARY KEY (host_id, bank_id, reflection_id);


--
-- Name: agent_memory_unit_entity_t agent_memory_unit_entity_t_pkey; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_memory_unit_entity_t
    ADD CONSTRAINT agent_memory_unit_entity_t_pkey PRIMARY KEY (host_id, bank_id, unit_id, entity_id);


--
-- Name: agent_memory_unit_t agent_memory_unit_t_pkey; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_memory_unit_t
    ADD CONSTRAINT agent_memory_unit_t_pkey PRIMARY KEY (host_id, bank_id, unit_id);


--
-- Name: agent_policy_snapshot_t agent_policy_snapshot_t_host_id_policy_digest_key; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_policy_snapshot_t
    ADD CONSTRAINT agent_policy_snapshot_t_host_id_policy_digest_key UNIQUE (host_id, policy_digest);


--
-- Name: agent_policy_snapshot_t agent_policy_snapshot_t_pkey; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_policy_snapshot_t
    ADD CONSTRAINT agent_policy_snapshot_t_pkey PRIMARY KEY (host_id, policy_snapshot_id);


--
-- Name: agent_quota_reservation_t agent_quota_reservation_t_pkey; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_quota_reservation_t
    ADD CONSTRAINT agent_quota_reservation_t_pkey PRIMARY KEY (host_id, quota_id, turn_id);


--
-- Name: agent_quota_usage_t agent_quota_usage_t_pkey; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_quota_usage_t
    ADD CONSTRAINT agent_quota_usage_t_pkey PRIMARY KEY (host_id, quota_id, window_start_ts);


--
-- Name: agent_session_event_t agent_session_event_t_host_id_session_id_event_sequence_key; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_session_event_t
    ADD CONSTRAINT agent_session_event_t_host_id_session_id_event_sequence_key UNIQUE (host_id, session_id, event_sequence);


--
-- Name: agent_session_event_t agent_session_event_t_pkey; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_session_event_t
    ADD CONSTRAINT agent_session_event_t_pkey PRIMARY KEY (host_id, event_id);


--
-- Name: agent_session_history_t agent_session_history_t_pkey; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_session_history_t
    ADD CONSTRAINT agent_session_history_t_pkey PRIMARY KEY (host_id, bank_id, session_id);


--
-- Name: agent_session_t agent_session_t_pkey; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_session_t
    ADD CONSTRAINT agent_session_t_pkey PRIMARY KEY (host_id, session_id);


--
-- Name: agent_turn_materialization_t agent_turn_materialization_t_host_id_turn_id_manifest_diges_key; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_turn_materialization_t
    ADD CONSTRAINT agent_turn_materialization_t_host_id_turn_id_manifest_diges_key UNIQUE (host_id, turn_id, manifest_digest);


--
-- Name: agent_turn_materialization_t agent_turn_materialization_t_pkey; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_turn_materialization_t
    ADD CONSTRAINT agent_turn_materialization_t_pkey PRIMARY KEY (host_id, turn_id);


--
-- Name: agent_turn_t agent_turn_t_host_id_session_id_client_message_id_key; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_turn_t
    ADD CONSTRAINT agent_turn_t_host_id_session_id_client_message_id_key UNIQUE (host_id, session_id, client_message_id);


--
-- Name: agent_turn_t agent_turn_t_host_id_session_id_idempotency_key_key; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_turn_t
    ADD CONSTRAINT agent_turn_t_host_id_session_id_idempotency_key_key UNIQUE (host_id, session_id, idempotency_key);


--
-- Name: agent_turn_t agent_turn_t_host_id_session_id_queue_sequence_key; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_turn_t
    ADD CONSTRAINT agent_turn_t_host_id_session_id_queue_sequence_key UNIQUE (host_id, session_id, queue_sequence);


--
-- Name: agent_turn_t agent_turn_t_host_id_session_id_turn_sequence_key; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_turn_t
    ADD CONSTRAINT agent_turn_t_host_id_session_id_turn_sequence_key UNIQUE (host_id, session_id, turn_sequence);


--
-- Name: agent_turn_t agent_turn_t_pkey; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_turn_t
    ADD CONSTRAINT agent_turn_t_pkey PRIMARY KEY (host_id, turn_id);


--
-- Name: operational_reference_evidence_t operational_reference_evidence_source_uk; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.operational_reference_evidence_t
    ADD CONSTRAINT operational_reference_evidence_source_uk UNIQUE (host_id, source_service, source_table, source_record_id, reference_kind);


--
-- Name: operational_reference_evidence_t operational_reference_evidence_t_pkey; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.operational_reference_evidence_t
    ADD CONSTRAINT operational_reference_evidence_t_pkey PRIMARY KEY (host_id, reference_id);


--
-- Name: operational_reference_reconciliation_t operational_reference_reconciliation_t_pkey; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.operational_reference_reconciliation_t
    ADD CONSTRAINT operational_reference_reconciliation_t_pkey PRIMARY KEY (host_id, reconciliation_id);


--
-- Name: runtime_operational_scope_t runtime_operational_scope_t_pkey; Type: CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.runtime_operational_scope_t
    ADD CONSTRAINT runtime_operational_scope_t_pkey PRIMARY KEY (host_id, service_id, instance_id);


--
-- Name: agent_action_pending_result_idx; Type: INDEX; Schema: agent_ops; Owner: -
--

CREATE INDEX agent_action_pending_result_idx ON agent_ops.agent_action_attempt_t USING btree (host_id, execution_attempt_id) WHERE ((execution_attempt_id IS NOT NULL) AND (origin_accepted_ts IS NULL));


--
-- Name: agent_action_result_event_uk; Type: INDEX; Schema: agent_ops; Owner: -
--

CREATE UNIQUE INDEX agent_action_result_event_uk ON agent_ops.agent_session_event_t USING btree (host_id, action_attempt_id) WHERE ((action_attempt_id IS NOT NULL) AND ((event_type)::text = 'ACTION_RESULT'::text));


--
-- Name: agent_approval_expiry_idx; Type: INDEX; Schema: agent_ops; Owner: -
--

CREATE INDEX agent_approval_expiry_idx ON agent_ops.agent_approval_t USING btree (expires_ts) WHERE ((state)::text = 'REQUESTED'::text);


--
-- Name: agent_delegation_replay_expiry_idx; Type: INDEX; Schema: agent_ops; Owner: -
--

CREATE INDEX agent_delegation_replay_expiry_idx ON agent_ops.agent_delegation_replay_t USING btree (expires_ts);


--
-- Name: agent_event_projection_idx; Type: INDEX; Schema: agent_ops; Owner: -
--

CREATE INDEX agent_event_projection_idx ON agent_ops.agent_session_event_t USING btree (host_id, session_id, event_sequence);


--
-- Name: agent_execution_outbox_pending_idx; Type: INDEX; Schema: agent_ops; Owner: -
--

CREATE INDEX agent_execution_outbox_pending_idx ON agent_ops.agent_execution_outbox_t USING btree (next_attempt_ts, created_ts) WHERE ((state)::text = 'PENDING'::text);


--
-- Name: agent_job_pending_idx; Type: INDEX; Schema: agent_ops; Owner: -
--

CREATE INDEX agent_job_pending_idx ON agent_ops.agent_job_t USING btree (state, deadline_ts) WHERE ((state)::text = ANY (ARRAY[('PENDING'::character varying)::text, ('TURN_CREATED'::character varying)::text, ('RUNNING'::character varying)::text]));


--
-- Name: agent_job_workflow_task_uk; Type: INDEX; Schema: agent_ops; Owner: -
--

CREATE UNIQUE INDEX agent_job_workflow_task_uk ON agent_ops.agent_job_t USING btree (host_id, workflow_process_id, workflow_task_id);


--
-- Name: agent_quota_reservation_pending_idx; Type: INDEX; Schema: agent_ops; Owner: -
--

CREATE INDEX agent_quota_reservation_pending_idx ON agent_ops.agent_quota_reservation_t USING btree (host_id, turn_id) WHERE (reconciled_ts IS NULL);


--
-- Name: agent_session_cleanup_idx; Type: INDEX; Schema: agent_ops; Owner: -
--

CREATE INDEX agent_session_cleanup_idx ON agent_ops.agent_session_t USING btree (cleanup_state, updated_ts) WHERE ((cleanup_state)::text = ANY (ARRAY[('CLEANUP_REQUESTED'::character varying)::text, ('CLEANUP_PENDING'::character varying)::text, ('CLEANUP_FAILED'::character varying)::text]));


--
-- Name: agent_session_expiry_idx; Type: INDEX; Schema: agent_ops; Owner: -
--

CREATE INDEX agent_session_expiry_idx ON agent_ops.agent_session_t USING btree (idle_expires_ts, maximum_expires_ts) WHERE ((state)::text = 'ACTIVE'::text);


--
-- Name: agent_session_pool_active_idx; Type: INDEX; Schema: agent_ops; Owner: -
--

CREATE INDEX agent_session_pool_active_idx ON agent_ops.agent_session_t USING btree (host_id, service_pool_id, state) WHERE ((state)::text = 'ACTIVE'::text);


--
-- Name: agent_session_resume_uk; Type: INDEX; Schema: agent_ops; Owner: -
--

CREATE UNIQUE INDEX agent_session_resume_uk ON agent_ops.agent_session_t USING btree (host_id, resume_handle_digest);


--
-- Name: agent_turn_execution_attempt_uk; Type: INDEX; Schema: agent_ops; Owner: -
--

CREATE UNIQUE INDEX agent_turn_execution_attempt_uk ON agent_ops.agent_turn_t USING btree (host_id, execution_attempt_id) WHERE (execution_attempt_id IS NOT NULL);


--
-- Name: agent_turn_fifo_idx; Type: INDEX; Schema: agent_ops; Owner: -
--

CREATE INDEX agent_turn_fifo_idx ON agent_ops.agent_turn_t USING btree (host_id, session_id, queue_sequence) WHERE ((state)::text = 'QUEUED'::text);


--
-- Name: agent_turn_one_active_uk; Type: INDEX; Schema: agent_ops; Owner: -
--

CREATE UNIQUE INDEX agent_turn_one_active_uk ON agent_ops.agent_turn_t USING btree (host_id, session_id) WHERE ((state)::text = ANY (ARRAY[('RECEIVED'::character varying)::text, ('RUNNING_MODEL'::character varying)::text, ('WAITING_ACTION'::character varying)::text, ('RUNNING_ACTION'::character varying)::text, ('WAITING_RECONCILIATION'::character varying)::text, ('WAITING_APPROVAL'::character varying)::text]));


--
-- Name: agent_turn_pool_queue_idx; Type: INDEX; Schema: agent_ops; Owner: -
--

CREATE INDEX agent_turn_pool_queue_idx ON agent_ops.agent_turn_t USING btree (host_id, service_pool_id, state, queue_sequence) WHERE ((state)::text = ANY (ARRAY[('QUEUED'::character varying)::text, ('RECEIVED'::character varying)::text, ('RUNNING_MODEL'::character varying)::text, ('WAITING_ACTION'::character varying)::text, ('RUNNING_ACTION'::character varying)::text, ('WAITING_RECONCILIATION'::character varying)::text, ('WAITING_APPROVAL'::character varying)::text]));


--
-- Name: agent_turn_reconcile_idx; Type: INDEX; Schema: agent_ops; Owner: -
--

CREATE INDEX agent_turn_reconcile_idx ON agent_ops.agent_turn_t USING btree (host_id, updated_ts) WHERE ((state)::text = ANY (ARRAY[('WAITING_RECONCILIATION'::character varying)::text, ('RUNNING_ACTION'::character varying)::text]));


--
-- Name: agent_turn_scheduling_request_uk; Type: INDEX; Schema: agent_ops; Owner: -
--

CREATE UNIQUE INDEX agent_turn_scheduling_request_uk ON agent_ops.agent_turn_t USING btree (host_id, scheduling_request_id) WHERE (scheduling_request_id IS NOT NULL);


--
-- Name: idx_mem_cooccur_e1; Type: INDEX; Schema: agent_ops; Owner: -
--

CREATE INDEX idx_mem_cooccur_e1 ON agent_ops.agent_memory_entity_cooccur_t USING btree (host_id, entity_id_1);


--
-- Name: idx_mem_cooccur_e2; Type: INDEX; Schema: agent_ops; Owner: -
--

CREATE INDEX idx_mem_cooccur_e2 ON agent_ops.agent_memory_entity_cooccur_t USING btree (host_id, entity_id_2);


--
-- Name: idx_mem_reflection_embedding; Type: INDEX; Schema: agent_ops; Owner: -
--

CREATE INDEX idx_mem_reflection_embedding ON agent_ops.agent_memory_reflection_t USING hnsw (embedding public.vector_cosine_ops);


--
-- Name: idx_mem_unit_bank; Type: INDEX; Schema: agent_ops; Owner: -
--

CREATE INDEX idx_mem_unit_bank ON agent_ops.agent_memory_unit_t USING btree (bank_id);


--
-- Name: idx_mem_unit_embedding; Type: INDEX; Schema: agent_ops; Owner: -
--

CREATE INDEX idx_mem_unit_embedding ON agent_ops.agent_memory_unit_t USING hnsw (embedding public.vector_cosine_ops);


--
-- Name: idx_session_bank; Type: INDEX; Schema: agent_ops; Owner: -
--

CREATE INDEX idx_session_bank ON agent_ops.agent_session_history_t USING btree (host_id, bank_id);


--
-- Name: operational_reference_evidence_reconcile_idx; Type: INDEX; Schema: agent_ops; Owner: -
--

CREATE INDEX operational_reference_evidence_reconcile_idx ON agent_ops.operational_reference_evidence_t USING btree (host_id, state, reconciled_ts);


--
-- Name: operational_reference_reconciliation_lookup_idx; Type: INDEX; Schema: agent_ops; Owner: -
--

CREATE INDEX operational_reference_reconciliation_lookup_idx ON agent_ops.operational_reference_reconciliation_t USING btree (host_id, reference_id, checked_ts DESC);


--
-- Name: agent_action_attempt_t agent_action_approval_fk; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_action_attempt_t
    ADD CONSTRAINT agent_action_approval_fk FOREIGN KEY (host_id, approval_id) REFERENCES agent_ops.agent_approval_t(host_id, approval_id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: agent_action_attempt_t agent_action_attempt_t_host_id_superseded_action_attempt_i_fkey; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_action_attempt_t
    ADD CONSTRAINT agent_action_attempt_t_host_id_superseded_action_attempt_i_fkey FOREIGN KEY (host_id, superseded_action_attempt_id) REFERENCES agent_ops.agent_action_attempt_t(host_id, action_attempt_id) ON DELETE RESTRICT;


--
-- Name: agent_action_attempt_t agent_action_attempt_t_host_id_turn_id_fkey; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_action_attempt_t
    ADD CONSTRAINT agent_action_attempt_t_host_id_turn_id_fkey FOREIGN KEY (host_id, turn_id) REFERENCES agent_ops.agent_turn_t(host_id, turn_id) ON DELETE CASCADE;


--
-- Name: agent_approval_t agent_approval_t_host_id_consumed_action_attempt_id_fkey; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_approval_t
    ADD CONSTRAINT agent_approval_t_host_id_consumed_action_attempt_id_fkey FOREIGN KEY (host_id, consumed_action_attempt_id) REFERENCES agent_ops.agent_action_attempt_t(host_id, action_attempt_id) ON DELETE RESTRICT;


--
-- Name: agent_approval_t agent_approval_t_host_id_turn_id_fkey; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_approval_t
    ADD CONSTRAINT agent_approval_t_host_id_turn_id_fkey FOREIGN KEY (host_id, turn_id) REFERENCES agent_ops.agent_turn_t(host_id, turn_id) ON DELETE CASCADE;


--
-- Name: agent_job_t agent_job_t_host_id_turn_id_fkey; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_job_t
    ADD CONSTRAINT agent_job_t_host_id_turn_id_fkey FOREIGN KEY (host_id, turn_id) REFERENCES agent_ops.agent_turn_t(host_id, turn_id);


--
-- Name: agent_memory_doc_t agent_memory_doc_t_host_id_bank_id_fkey; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_memory_doc_t
    ADD CONSTRAINT agent_memory_doc_t_host_id_bank_id_fkey FOREIGN KEY (host_id, bank_id) REFERENCES agent_ops.agent_memory_bank_t(host_id, bank_id) ON DELETE CASCADE;


--
-- Name: agent_memory_entity_cooccur_t agent_memory_entity_cooccur_t_host_id_bank_id_entity_id_1_fkey; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_memory_entity_cooccur_t
    ADD CONSTRAINT agent_memory_entity_cooccur_t_host_id_bank_id_entity_id_1_fkey FOREIGN KEY (host_id, bank_id, entity_id_1) REFERENCES agent_ops.agent_memory_entity_t(host_id, bank_id, entity_id) ON DELETE CASCADE;


--
-- Name: agent_memory_entity_cooccur_t agent_memory_entity_cooccur_t_host_id_bank_id_entity_id_2_fkey; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_memory_entity_cooccur_t
    ADD CONSTRAINT agent_memory_entity_cooccur_t_host_id_bank_id_entity_id_2_fkey FOREIGN KEY (host_id, bank_id, entity_id_2) REFERENCES agent_ops.agent_memory_entity_t(host_id, bank_id, entity_id) ON DELETE CASCADE;


--
-- Name: agent_memory_entity_t agent_memory_entity_t_host_id_bank_id_fkey; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_memory_entity_t
    ADD CONSTRAINT agent_memory_entity_t_host_id_bank_id_fkey FOREIGN KEY (host_id, bank_id) REFERENCES agent_ops.agent_memory_bank_t(host_id, bank_id) ON DELETE CASCADE;


--
-- Name: agent_memory_link_t agent_memory_link_t_host_id_bank_id_from_unit_id_fkey; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_memory_link_t
    ADD CONSTRAINT agent_memory_link_t_host_id_bank_id_from_unit_id_fkey FOREIGN KEY (host_id, bank_id, from_unit_id) REFERENCES agent_ops.agent_memory_unit_t(host_id, bank_id, unit_id) ON DELETE CASCADE;


--
-- Name: agent_memory_link_t agent_memory_link_t_host_id_bank_id_to_unit_id_fkey; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_memory_link_t
    ADD CONSTRAINT agent_memory_link_t_host_id_bank_id_to_unit_id_fkey FOREIGN KEY (host_id, bank_id, to_unit_id) REFERENCES agent_ops.agent_memory_unit_t(host_id, bank_id, unit_id) ON DELETE CASCADE;


--
-- Name: agent_memory_reflection_t agent_memory_reflection_t_host_id_bank_id_fkey; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_memory_reflection_t
    ADD CONSTRAINT agent_memory_reflection_t_host_id_bank_id_fkey FOREIGN KEY (host_id, bank_id) REFERENCES agent_ops.agent_memory_bank_t(host_id, bank_id) ON DELETE CASCADE;


--
-- Name: agent_memory_unit_entity_t agent_memory_unit_entity_t_host_id_bank_id_entity_id_fkey; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_memory_unit_entity_t
    ADD CONSTRAINT agent_memory_unit_entity_t_host_id_bank_id_entity_id_fkey FOREIGN KEY (host_id, bank_id, entity_id) REFERENCES agent_ops.agent_memory_entity_t(host_id, bank_id, entity_id) ON DELETE CASCADE;


--
-- Name: agent_memory_unit_entity_t agent_memory_unit_entity_t_host_id_bank_id_unit_id_fkey; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_memory_unit_entity_t
    ADD CONSTRAINT agent_memory_unit_entity_t_host_id_bank_id_unit_id_fkey FOREIGN KEY (host_id, bank_id, unit_id) REFERENCES agent_ops.agent_memory_unit_t(host_id, bank_id, unit_id) ON DELETE CASCADE;


--
-- Name: agent_memory_unit_t agent_memory_unit_t_host_id_bank_id_doc_id_fkey; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_memory_unit_t
    ADD CONSTRAINT agent_memory_unit_t_host_id_bank_id_doc_id_fkey FOREIGN KEY (host_id, bank_id, doc_id) REFERENCES agent_ops.agent_memory_doc_t(host_id, bank_id, doc_id) ON DELETE CASCADE;


--
-- Name: agent_memory_unit_t agent_memory_unit_t_host_id_bank_id_fkey; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_memory_unit_t
    ADD CONSTRAINT agent_memory_unit_t_host_id_bank_id_fkey FOREIGN KEY (host_id, bank_id) REFERENCES agent_ops.agent_memory_bank_t(host_id, bank_id) ON DELETE CASCADE;


--
-- Name: agent_quota_reservation_t agent_quota_reservation_t_host_id_quota_id_window_start_ts_fkey; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_quota_reservation_t
    ADD CONSTRAINT agent_quota_reservation_t_host_id_quota_id_window_start_ts_fkey FOREIGN KEY (host_id, quota_id, window_start_ts) REFERENCES agent_ops.agent_quota_usage_t(host_id, quota_id, window_start_ts) ON DELETE CASCADE;


--
-- Name: agent_quota_reservation_t agent_quota_reservation_t_host_id_turn_id_fkey; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_quota_reservation_t
    ADD CONSTRAINT agent_quota_reservation_t_host_id_turn_id_fkey FOREIGN KEY (host_id, turn_id) REFERENCES agent_ops.agent_turn_t(host_id, turn_id) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED;


--
-- Name: agent_session_t agent_session_active_turn_fk; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_session_t
    ADD CONSTRAINT agent_session_active_turn_fk FOREIGN KEY (host_id, active_turn_id) REFERENCES agent_ops.agent_turn_t(host_id, turn_id) DEFERRABLE INITIALLY DEFERRED;


--
-- Name: agent_session_event_t agent_session_event_t_host_id_action_attempt_id_fkey; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_session_event_t
    ADD CONSTRAINT agent_session_event_t_host_id_action_attempt_id_fkey FOREIGN KEY (host_id, action_attempt_id) REFERENCES agent_ops.agent_action_attempt_t(host_id, action_attempt_id) ON DELETE RESTRICT;


--
-- Name: agent_session_event_t agent_session_event_t_host_id_session_id_fkey; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_session_event_t
    ADD CONSTRAINT agent_session_event_t_host_id_session_id_fkey FOREIGN KEY (host_id, session_id) REFERENCES agent_ops.agent_session_t(host_id, session_id) ON DELETE CASCADE;


--
-- Name: agent_session_event_t agent_session_event_t_host_id_turn_id_fkey; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_session_event_t
    ADD CONSTRAINT agent_session_event_t_host_id_turn_id_fkey FOREIGN KEY (host_id, turn_id) REFERENCES agent_ops.agent_turn_t(host_id, turn_id) ON DELETE CASCADE;


--
-- Name: agent_session_history_t agent_session_history_t_host_id_bank_id_fkey; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_session_history_t
    ADD CONSTRAINT agent_session_history_t_host_id_bank_id_fkey FOREIGN KEY (host_id, bank_id) REFERENCES agent_ops.agent_memory_bank_t(host_id, bank_id) ON DELETE CASCADE;


--
-- Name: agent_session_t agent_session_t_host_id_bank_id_fkey; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_session_t
    ADD CONSTRAINT agent_session_t_host_id_bank_id_fkey FOREIGN KEY (host_id, bank_id) REFERENCES agent_ops.agent_memory_bank_t(host_id, bank_id) ON DELETE RESTRICT;


--
-- Name: agent_session_t agent_session_t_host_id_policy_snapshot_id_fkey; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_session_t
    ADD CONSTRAINT agent_session_t_host_id_policy_snapshot_id_fkey FOREIGN KEY (host_id, policy_snapshot_id) REFERENCES agent_ops.agent_policy_snapshot_t(host_id, policy_snapshot_id) ON DELETE RESTRICT;


--
-- Name: agent_turn_materialization_t agent_turn_materialization_t_host_id_turn_id_fkey; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_turn_materialization_t
    ADD CONSTRAINT agent_turn_materialization_t_host_id_turn_id_fkey FOREIGN KEY (host_id, turn_id) REFERENCES agent_ops.agent_turn_t(host_id, turn_id) ON DELETE CASCADE;


--
-- Name: agent_turn_t agent_turn_t_host_id_policy_snapshot_id_fkey; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_turn_t
    ADD CONSTRAINT agent_turn_t_host_id_policy_snapshot_id_fkey FOREIGN KEY (host_id, policy_snapshot_id) REFERENCES agent_ops.agent_policy_snapshot_t(host_id, policy_snapshot_id) ON DELETE RESTRICT;


--
-- Name: agent_turn_t agent_turn_t_host_id_session_id_fkey; Type: FK CONSTRAINT; Schema: agent_ops; Owner: -
--

ALTER TABLE ONLY agent_ops.agent_turn_t
    ADD CONSTRAINT agent_turn_t_host_id_session_id_fkey FOREIGN KEY (host_id, session_id) REFERENCES agent_ops.agent_session_t(host_id, session_id) ON DELETE CASCADE;


RESET ROLE;

GRANT USAGE ON SCHEMA agent_ops TO operations_agent_runtime;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA agent_ops
    TO operations_agent_runtime;
GRANT USAGE, SELECT, UPDATE ON ALL SEQUENCES IN SCHEMA agent_ops
    TO operations_agent_runtime;
