-- Generated deterministically from portal-db/postgres/ddl.sql.
-- Workflow operational authority is reset/reseeded in early development; this
-- migration deliberately contains no source-row copy or dual-write machinery.

SET search_path TO workflow_ops, pg_catalog;

CREATE TABLE workflow_ops.process_info_t (
    host_id uuid NOT NULL,
    process_id uuid NOT NULL,
    wf_def_id uuid NOT NULL,
    wf_instance_id character varying(126) NOT NULL,
    app_id character varying(512) NOT NULL,
    process_type character varying(126) NOT NULL,
    status_code character(1) NOT NULL,
    started_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    ex_trigger_ts timestamp with time zone NOT NULL,
    custom_status_code character varying(126),
    completed_ts timestamp with time zone,
    result_code character varying(126),
    source_id character varying(126),
    branch_code character varying(126),
    rr_code character varying(126),
    party_id character varying(126),
    party_name character varying(126),
    counter_party_id character varying(126),
    counter_party_name character varying(126),
    txn_id character varying(126),
    txn_name character varying(126),
    product_id character varying(126),
    product_name character varying(126),
    product_type character varying(126),
    group_name character varying(126),
    subgroup_name character varying(126),
    event_start_ts timestamp with time zone,
    event_end_ts timestamp with time zone,
    event_other_ts timestamp with time zone,
    event_other character varying(126),
    risk numeric,
    risk_scale integer,
    price numeric,
    price_scale integer,
    product_qy numeric,
    currency_code character(3),
    ex_ref_id character varying(126),
    ex_ref_code character varying(126),
    product_qy_scale integer,
    parent_process_id character varying(22),
    deadline_ts timestamp with time zone,
    parent_group_id numeric,
    process_subtype_code character varying(126),
    owning_group_name character varying(126),
    input_data jsonb,
    context_data jsonb,
    error_info text,
    aggregate_version bigint DEFAULT 1 NOT NULL,
    active boolean DEFAULT true,
    update_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP,
    update_user character varying(126) DEFAULT SESSION_USER,
    definition_snapshot jsonb,
    definition_digest character varying(64),
    policy_snapshot_id uuid,
    policy_digest character varying(64),
    source_event_id character varying(126),
    execution_profile_id character varying(126)
);

CREATE TABLE workflow_ops.task_info_t (
    host_id uuid NOT NULL,
    task_id uuid NOT NULL,
    task_type character varying(126) NOT NULL,
    process_id uuid NOT NULL,
    wf_instance_id character varying(126) NOT NULL,
    wf_task_id character varying(126) NOT NULL,
    status_code character(1) NOT NULL,
    started_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    locked character(1) NOT NULL,
    priority integer NOT NULL,
    completed_ts timestamp with time zone,
    completed_user character varying(126),
    result_code character varying(126),
    locking_user character varying(126),
    locking_role character varying(126),
    deadline_ts timestamp with time zone,
    lock_group character varying(126),
    task_input jsonb,
    task_output jsonb,
    aggregate_version bigint DEFAULT 1 NOT NULL,
    active boolean DEFAULT true,
    update_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP,
    update_user character varying(126) DEFAULT SESSION_USER,
    execution_placement character varying(16) DEFAULT 'host'::character varying NOT NULL,
    task_policy_digest character varying(64),
    scheduling_request_id uuid,
    accepted_attempt integer,
    execution_class character varying(16) DEFAULT 'standard'::character varying NOT NULL,
    lease_owner uuid,
    lease_fencing_token bigint DEFAULT 0 NOT NULL,
    lease_expires_ts timestamp with time zone,
    attempt_no integer DEFAULT 1 NOT NULL,
    maximum_attempts integer DEFAULT 1 NOT NULL,
    next_attempt_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    effect_state character varying(16) DEFAULT 'none'::character varying NOT NULL,
    downstream_idempotency_key character varying(255),
    compensation_task character varying(255),
    is_compensation boolean DEFAULT false NOT NULL,
    fork_join_id uuid,
    branch_name character varying(255),
    CONSTRAINT task_info_execution_class_v1_ck CHECK (((execution_class)::text = ANY (ARRAY[('interactive'::character varying)::text, ('standard'::character varying)::text, ('batch'::character varying)::text]))),
    CONSTRAINT task_info_execution_placement_ck CHECK (((execution_placement)::text = ANY (ARRAY[('host'::character varying)::text, ('runner'::character varying)::text]))),
    CONSTRAINT task_info_t_attempt_no_check CHECK ((attempt_no > 0)),
    CONSTRAINT task_info_t_effect_state_check CHECK (((effect_state)::text = ANY (ARRAY[('none'::character varying)::text, ('possible'::character varying)::text, ('confirmed'::character varying)::text]))),
    CONSTRAINT task_info_t_maximum_attempts_check CHECK ((maximum_attempts > 0))
);

CREATE TABLE workflow_ops.workflow_approval_t (
    host_id uuid NOT NULL,
    approval_id uuid NOT NULL,
    process_id uuid NOT NULL,
    task_id uuid NOT NULL,
    preceding_execution_id uuid,
    consuming_execution_id uuid,
    artifact_digest_set jsonb DEFAULT '[]'::jsonb NOT NULL,
    provenance_digest character varying(128),
    target character varying(255) NOT NULL,
    operation character varying(126) NOT NULL,
    policy_digest character varying(64) NOT NULL,
    state character varying(32) NOT NULL,
    actor character varying(255),
    reason text,
    requested_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    decided_ts timestamp with time zone,
    expires_ts timestamp with time zone NOT NULL,
    CONSTRAINT workflow_approval_t_check CHECK (((consuming_execution_id IS NULL) OR ((state)::text = 'CONSUMED'::text))),
    CONSTRAINT workflow_approval_t_state_check CHECK (((state)::text = ANY (ARRAY[('REQUESTED'::character varying)::text, ('APPROVED'::character varying)::text, ('REJECTED'::character varying)::text, ('EXPIRED'::character varying)::text, ('CONSUMED'::character varying)::text])))
);

CREATE TABLE workflow_ops.workflow_artifact_t (
    host_id uuid NOT NULL,
    artifact_id uuid NOT NULL,
    execution_id uuid NOT NULL,
    execution_session_id uuid,
    process_id uuid,
    task_id uuid,
    logical_name character varying(255) NOT NULL,
    media_type character varying(255) NOT NULL,
    size_bytes bigint NOT NULL,
    content_digest character varying(128) NOT NULL,
    storage_reference text NOT NULL,
    producer character varying(255) NOT NULL,
    policy_digest character varying(64) NOT NULL,
    provenance_reference text,
    signature_reference text,
    retain_until_ts timestamp with time zone NOT NULL,
    legal_hold boolean DEFAULT false NOT NULL,
    verification_state character varying(32) NOT NULL,
    deletion_state character varying(32) DEFAULT 'RETAINED'::character varying NOT NULL,
    deletion_attempt integer DEFAULT 0 NOT NULL,
    deletion_next_retry_ts timestamp with time zone,
    deletion_evidence jsonb,
    deleted_ts timestamp with time zone,
    created_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    staging_reference text,
    promotion_state character varying(32) DEFAULT 'BOUND'::character varying NOT NULL,
    provenance_digest character varying(128),
    CONSTRAINT workflow_artifact_promotion_state_ck CHECK (((promotion_state)::text = ANY (ARRAY[('STAGED'::character varying)::text, ('METADATA_COMMITTED'::character varying)::text, ('BOUND'::character varying)::text, ('QUARANTINED'::character varying)::text]))),
    CONSTRAINT workflow_artifact_t_deletion_attempt_check CHECK ((deletion_attempt >= 0)),
    CONSTRAINT workflow_artifact_t_deletion_state_check CHECK (((deletion_state)::text = ANY (ARRAY[('RETAINED'::character varying)::text, ('DELETE_PENDING'::character varying)::text, ('DELETING'::character varying)::text, ('DELETED'::character varying)::text, ('DELETE_FAILED'::character varying)::text]))),
    CONSTRAINT workflow_artifact_t_size_bytes_check CHECK ((size_bytes >= 0)),
    CONSTRAINT workflow_artifact_t_verification_state_check CHECK (((verification_state)::text = ANY (ARRAY[('PENDING'::character varying)::text, ('VERIFIED'::character varying)::text, ('REJECTED'::character varying)::text])))
);

CREATE TABLE workflow_ops.workflow_executor_tenant_turn_t (
    host_id uuid NOT NULL,
    last_claim_ts timestamp with time zone DEFAULT '-infinity'::timestamp with time zone NOT NULL,
    claim_count bigint DEFAULT 0 NOT NULL,
    updated_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    CONSTRAINT workflow_executor_tenant_turn_t_claim_count_check CHECK ((claim_count >= 0))
);

CREATE TABLE workflow_ops.workflow_fork_branch_t (
    host_id uuid NOT NULL,
    join_id uuid NOT NULL,
    branch_name character varying(255) NOT NULL,
    task_id uuid NOT NULL,
    state character varying(16) DEFAULT 'RUNNING'::character varying NOT NULL,
    result jsonb,
    completed_ts timestamp with time zone,
    CONSTRAINT workflow_fork_branch_t_state_check CHECK (((state)::text = ANY (ARRAY[('RUNNING'::character varying)::text, ('COMPLETED'::character varying)::text, ('FAILED'::character varying)::text, ('CANCELLED'::character varying)::text])))
);

CREATE TABLE workflow_ops.workflow_fork_join_t (
    host_id uuid NOT NULL,
    join_id uuid NOT NULL,
    workflow_instance_id uuid NOT NULL,
    process_id uuid NOT NULL,
    fork_task_id uuid NOT NULL,
    fork_task_name character varying(255) NOT NULL,
    continuation_task character varying(255),
    compete boolean DEFAULT false NOT NULL,
    expected_branches integer NOT NULL,
    completed_branches integer DEFAULT 0 NOT NULL,
    failed_branches integer DEFAULT 0 NOT NULL,
    state character varying(16) DEFAULT 'RUNNING'::character varying NOT NULL,
    branch_results jsonb DEFAULT '{}'::jsonb NOT NULL,
    created_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    completed_ts timestamp with time zone,
    CONSTRAINT workflow_fork_join_t_branch_results_check CHECK ((jsonb_typeof(branch_results) = 'object'::text)),
    CONSTRAINT workflow_fork_join_t_completed_branches_check CHECK ((completed_branches >= 0)),
    CONSTRAINT workflow_fork_join_t_expected_branches_check CHECK (((expected_branches >= 1) AND (expected_branches <= 64))),
    CONSTRAINT workflow_fork_join_t_failed_branches_check CHECK ((failed_branches >= 0)),
    CONSTRAINT workflow_fork_join_t_state_check CHECK (((state)::text = ANY (ARRAY[('RUNNING'::character varying)::text, ('COMPLETED'::character varying)::text, ('FAILED'::character varying)::text, ('CANCELLED'::character varying)::text])))
);

CREATE TABLE workflow_ops.workflow_invocation_audit_outbox_t (
    host_id uuid NOT NULL,
    event_id uuid NOT NULL,
    workflow_instance_id uuid NOT NULL,
    event_type character varying(126) NOT NULL,
    payload jsonb NOT NULL,
    correlation_id character varying(255) NOT NULL,
    event_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    published_ts timestamp with time zone,
    CONSTRAINT workflow_invocation_audit_outbox_t_payload_check CHECK ((jsonb_typeof(payload) = 'object'::text))
);

CREATE TABLE workflow_ops.workflow_invocation_budget_reservation_t (
    host_id uuid NOT NULL,
    reservation_id uuid NOT NULL,
    ledger_id uuid NOT NULL,
    generation bigint NOT NULL,
    fencing_token bigint NOT NULL,
    task_attempts bigint NOT NULL,
    nested_calls bigint NOT NULL,
    reserved_bytes bigint NOT NULL,
    reserved_cost_units bigint NOT NULL,
    actual_bytes bigint,
    actual_cost_units bigint,
    state character varying(16) NOT NULL,
    created_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    reconciled_ts timestamp with time zone,
    CONSTRAINT workflow_invocation_budget_reservatio_reserved_cost_units_check CHECK ((reserved_cost_units >= 0)),
    CONSTRAINT workflow_invocation_budget_reservation_t_check CHECK (((actual_bytes IS NULL) OR ((actual_bytes >= 0) AND (actual_bytes <= reserved_bytes)))),
    CONSTRAINT workflow_invocation_budget_reservation_t_check1 CHECK (((actual_cost_units IS NULL) OR ((actual_cost_units >= 0) AND (actual_cost_units <= reserved_cost_units)))),
    CONSTRAINT workflow_invocation_budget_reservation_t_fencing_token_check CHECK ((fencing_token > 0)),
    CONSTRAINT workflow_invocation_budget_reservation_t_generation_check CHECK ((generation > 0)),
    CONSTRAINT workflow_invocation_budget_reservation_t_nested_calls_check CHECK ((nested_calls >= 0)),
    CONSTRAINT workflow_invocation_budget_reservation_t_reserved_bytes_check CHECK ((reserved_bytes >= 0)),
    CONSTRAINT workflow_invocation_budget_reservation_t_state_check CHECK (((state)::text = ANY (ARRAY[('RESERVED'::character varying)::text, ('RECONCILED'::character varying)::text, ('RELEASED'::character varying)::text]))),
    CONSTRAINT workflow_invocation_budget_reservation_t_task_attempts_check CHECK ((task_attempts >= 0))
);

CREATE TABLE workflow_ops.workflow_invocation_budget_t (
    host_id uuid NOT NULL,
    ledger_id uuid NOT NULL,
    workflow_instance_id uuid NOT NULL,
    generation bigint DEFAULT 1 NOT NULL,
    task_attempt_limit bigint NOT NULL,
    nested_call_limit bigint NOT NULL,
    byte_limit bigint NOT NULL,
    cost_unit_limit bigint NOT NULL,
    task_attempt_used bigint DEFAULT 0 NOT NULL,
    nested_call_used bigint DEFAULT 0 NOT NULL,
    byte_used bigint DEFAULT 0 NOT NULL,
    cost_unit_used bigint DEFAULT 0 NOT NULL,
    task_attempt_reserved bigint DEFAULT 0 NOT NULL,
    nested_call_reserved bigint DEFAULT 0 NOT NULL,
    byte_reserved bigint DEFAULT 0 NOT NULL,
    cost_unit_reserved bigint DEFAULT 0 NOT NULL,
    deadline_ts timestamp with time zone NOT NULL,
    updated_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    request_byte_limit bigint DEFAULT 1048576 NOT NULL,
    result_byte_limit bigint DEFAULT 1048576 NOT NULL,
    CONSTRAINT workflow_invocation_budget_t_byte_limit_check CHECK ((byte_limit > 0)),
    CONSTRAINT workflow_invocation_budget_t_byte_reserved_check CHECK ((byte_reserved >= 0)),
    CONSTRAINT workflow_invocation_budget_t_byte_used_check CHECK ((byte_used >= 0)),
    CONSTRAINT workflow_invocation_budget_t_check CHECK (((task_attempt_used + task_attempt_reserved) <= task_attempt_limit)),
    CONSTRAINT workflow_invocation_budget_t_check1 CHECK (((nested_call_used + nested_call_reserved) <= nested_call_limit)),
    CONSTRAINT workflow_invocation_budget_t_check2 CHECK (((byte_used + byte_reserved) <= byte_limit)),
    CONSTRAINT workflow_invocation_budget_t_check3 CHECK (((cost_unit_used + cost_unit_reserved) <= cost_unit_limit)),
    CONSTRAINT workflow_invocation_budget_t_cost_unit_limit_check CHECK ((cost_unit_limit >= 0)),
    CONSTRAINT workflow_invocation_budget_t_cost_unit_reserved_check CHECK ((cost_unit_reserved >= 0)),
    CONSTRAINT workflow_invocation_budget_t_cost_unit_used_check CHECK ((cost_unit_used >= 0)),
    CONSTRAINT workflow_invocation_budget_t_generation_check CHECK ((generation > 0)),
    CONSTRAINT workflow_invocation_budget_t_nested_call_limit_check CHECK ((nested_call_limit >= 0)),
    CONSTRAINT workflow_invocation_budget_t_nested_call_reserved_check CHECK ((nested_call_reserved >= 0)),
    CONSTRAINT workflow_invocation_budget_t_nested_call_used_check CHECK ((nested_call_used >= 0)),
    CONSTRAINT workflow_invocation_budget_t_request_byte_limit_check CHECK ((request_byte_limit > 0)),
    CONSTRAINT workflow_invocation_budget_t_result_byte_limit_check CHECK ((result_byte_limit > 0)),
    CONSTRAINT workflow_invocation_budget_t_task_attempt_limit_check CHECK ((task_attempt_limit > 0)),
    CONSTRAINT workflow_invocation_budget_t_task_attempt_reserved_check CHECK ((task_attempt_reserved >= 0)),
    CONSTRAINT workflow_invocation_budget_t_task_attempt_used_check CHECK ((task_attempt_used >= 0))
);

CREATE TABLE workflow_ops.workflow_invocation_event_quarantine_t (
    host_id uuid NOT NULL,
    quarantine_id uuid NOT NULL,
    consumer_group character varying(255) NOT NULL,
    partition_id integer NOT NULL,
    source_offset bigint NOT NULL,
    aggregate_id character varying(255) NOT NULL,
    aggregate_version bigint NOT NULL,
    transaction_id uuid,
    payload_digest character varying(71) NOT NULL,
    encrypted_payload bytea,
    immutable_payload_reference text,
    failure_code character varying(126) NOT NULL,
    failure_detail text NOT NULL,
    attempt_count integer DEFAULT 0 NOT NULL,
    repaired_by character varying(255),
    repair_reason text,
    replay_state character varying(16) DEFAULT 'BLOCKED'::character varying NOT NULL,
    created_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    resolved_ts timestamp with time zone,
    CONSTRAINT workflow_invocation_event_quarantine_t_attempt_count_check CHECK ((attempt_count >= 0)),
    CONSTRAINT workflow_invocation_event_quarantine_t_check CHECK (((encrypted_payload IS NOT NULL) <> (immutable_payload_reference IS NOT NULL))),
    CONSTRAINT workflow_invocation_event_quarantine_t_payload_digest_check CHECK (((payload_digest)::text ~ '^sha256:[0-9a-f]{64}$'::text)),
    CONSTRAINT workflow_invocation_event_quarantine_t_replay_state_check CHECK (((replay_state)::text = ANY (ARRAY[('BLOCKED'::character varying)::text, ('REPAIRED'::character varying)::text, ('REPLAYED'::character varying)::text, ('DISCARDED'::character varying)::text])))
);

CREATE TABLE workflow_ops.workflow_invocation_idempotency_t (
    host_id uuid NOT NULL,
    reservation_id uuid NOT NULL,
    scope_digest character varying(71) NOT NULL,
    idempotency_kind character varying(16) NOT NULL,
    stable_tool_ref uuid NOT NULL,
    principal_subject character varying(255) NOT NULL,
    end_user_subject character varying(255) NOT NULL,
    workflow_instance_id uuid NOT NULL,
    definition_digest character varying(71) NOT NULL,
    input_digest character varying(71) NOT NULL,
    generation bigint DEFAULT 1 NOT NULL,
    in_flight_until timestamp with time zone NOT NULL,
    result_replay_until timestamp with time zone NOT NULL,
    active boolean DEFAULT true NOT NULL,
    created_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    CONSTRAINT workflow_invocation_idempotency_t_check CHECK ((result_replay_until >= in_flight_until)),
    CONSTRAINT workflow_invocation_idempotency_t_definition_digest_check CHECK (((definition_digest)::text ~ '^sha256:[0-9a-f]{64}$'::text)),
    CONSTRAINT workflow_invocation_idempotency_t_generation_check CHECK ((generation > 0)),
    CONSTRAINT workflow_invocation_idempotency_t_idempotency_kind_check CHECK (((idempotency_kind)::text = ANY (ARRAY[('DERIVED'::character varying)::text, ('EXPLICIT'::character varying)::text, ('BUSINESS'::character varying)::text]))),
    CONSTRAINT workflow_invocation_idempotency_t_input_digest_check CHECK (((input_digest)::text ~ '^sha256:[0-9a-f]{64}$'::text)),
    CONSTRAINT workflow_invocation_idempotency_t_scope_digest_check CHECK (((scope_digest)::text ~ '^sha256:[0-9a-f]{64}$'::text))
);

CREATE TABLE workflow_ops.workflow_invocation_t (
    host_id uuid NOT NULL,
    workflow_instance_id uuid NOT NULL,
    binding_id uuid NOT NULL,
    process_id uuid,
    stable_tool_ref uuid NOT NULL,
    wf_def_id uuid NOT NULL,
    workflow_version character varying(64) NOT NULL,
    definition_digest character varying(71) NOT NULL,
    schema_digest character varying(71) NOT NULL,
    policy_digest character varying(71) NOT NULL,
    response_policy_digest character varying(71) NOT NULL,
    principal_subject character varying(255) NOT NULL,
    end_user_subject character varying(255) NOT NULL,
    input jsonb NOT NULL,
    input_digest character varying(71) NOT NULL,
    canonical_input_profile character varying(32) NOT NULL,
    invocation_mode character varying(8) NOT NULL,
    execution_class character varying(16) NOT NULL,
    permit_depth integer DEFAULT 0 NOT NULL,
    state character varying(16) NOT NULL,
    effect_state character varying(16) DEFAULT 'none'::character varying NOT NULL,
    public_result jsonb,
    normalized_error jsonb,
    correlation_id character varying(255) NOT NULL,
    deadline_ts timestamp with time zone NOT NULL,
    accepted_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    terminal_ts timestamp with time zone,
    state_version bigint DEFAULT 1 NOT NULL,
    subject_claims jsonb DEFAULT '{}'::jsonb NOT NULL,
    cancellation_policy character varying(32) DEFAULT 'BEFORE_EFFECTS_ONLY'::character varying NOT NULL,
    cancel_requested_ts timestamp with time zone,
    non_cancellable_reason text,
    response_policy_snapshot jsonb DEFAULT '{}'::jsonb NOT NULL,
    user_authorization text,
    user_authorization_exp bigint,
    CONSTRAINT workflow_invocation_state_v2_ck CHECK (((state)::text = ANY (ARRAY[('ACCEPTED'::character varying)::text, ('RUNNING'::character varying)::text, ('WAITING'::character varying)::text, ('COMPENSATING'::character varying)::text, ('COMPLETED'::character varying)::text, ('FAILED'::character varying)::text, ('CANCELLED'::character varying)::text]))),
    CONSTRAINT workflow_invocation_t_cancellation_policy_check CHECK (((cancellation_policy)::text = ANY (ARRAY[('BEFORE_EFFECTS_ONLY'::character varying)::text, ('COOPERATIVE'::character varying)::text, ('DISABLED'::character varying)::text]))),
    CONSTRAINT workflow_invocation_t_canonical_input_profile_check CHECK (((canonical_input_profile)::text = 'rfc8785-safe-json-v1'::text)),
    CONSTRAINT workflow_invocation_t_check CHECK ((((invocation_mode)::text <> 'sync'::text) OR ((execution_class)::text = 'interactive'::text))),
    CONSTRAINT workflow_invocation_t_check1 CHECK ((((state)::text = ANY (ARRAY[('COMPLETED'::character varying)::text, ('FAILED'::character varying)::text, ('CANCELLED'::character varying)::text])) = (terminal_ts IS NOT NULL))),
    CONSTRAINT workflow_invocation_t_definition_digest_check CHECK (((definition_digest)::text ~ '^sha256:[0-9a-f]{64}$'::text)),
    CONSTRAINT workflow_invocation_t_effect_state_check CHECK (((effect_state)::text = ANY (ARRAY[('none'::character varying)::text, ('possible'::character varying)::text, ('confirmed'::character varying)::text]))),
    CONSTRAINT workflow_invocation_t_execution_class_check CHECK (((execution_class)::text = ANY (ARRAY[('interactive'::character varying)::text, ('standard'::character varying)::text, ('batch'::character varying)::text]))),
    CONSTRAINT workflow_invocation_t_input_check CHECK ((jsonb_typeof(input) = 'object'::text)),
    CONSTRAINT workflow_invocation_t_input_digest_check CHECK (((input_digest)::text ~ '^sha256:[0-9a-f]{64}$'::text)),
    CONSTRAINT workflow_invocation_t_invocation_mode_check CHECK (((invocation_mode)::text = ANY (ARRAY[('sync'::character varying)::text, ('async'::character varying)::text]))),
    CONSTRAINT workflow_invocation_t_normalized_error_check CHECK (((normalized_error IS NULL) OR (jsonb_typeof(normalized_error) = 'object'::text))),
    CONSTRAINT workflow_invocation_t_permit_depth_check CHECK (((permit_depth >= 0) AND (permit_depth <= 16))),
    CONSTRAINT workflow_invocation_t_policy_digest_check CHECK (((policy_digest)::text ~ '^sha256:[0-9a-f]{64}$'::text)),
    CONSTRAINT workflow_invocation_t_public_result_check CHECK (((public_result IS NULL) OR (jsonb_typeof(public_result) = 'object'::text))),
    CONSTRAINT workflow_invocation_t_response_policy_digest_check CHECK (((response_policy_digest)::text ~ '^sha256:[0-9a-f]{64}$'::text)),
    CONSTRAINT workflow_invocation_t_response_policy_snapshot_check CHECK ((jsonb_typeof(response_policy_snapshot) = 'object'::text)),
    CONSTRAINT workflow_invocation_t_schema_digest_check CHECK (((schema_digest)::text ~ '^sha256:[0-9a-f]{64}$'::text)),
    CONSTRAINT workflow_invocation_t_state_version_check CHECK ((state_version > 0)),
    CONSTRAINT workflow_invocation_t_subject_claims_check CHECK ((jsonb_typeof(subject_claims) = 'object'::text)),
    CONSTRAINT workflow_invocation_t_user_authorization_check CHECK (((user_authorization IS NULL) OR (user_authorization ~ '^Bearer [^[:space:]]+$'::text))),
    CONSTRAINT workflow_invocation_t_user_authorization_exp_check CHECK (((user_authorization_exp IS NULL) OR (user_authorization_exp > 0)))
);

CREATE TABLE workflow_ops.workflow_task_effect_t (
    host_id uuid NOT NULL,
    workflow_instance_id uuid NOT NULL,
    task_name character varying(255) NOT NULL,
    idempotency_key character varying(255) NOT NULL,
    request_digest character varying(71) NOT NULL,
    effect_state character varying(16) DEFAULT 'possible'::character varying NOT NULL,
    result jsonb,
    first_attempt_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    confirmed_ts timestamp with time zone,
    CONSTRAINT workflow_task_effect_t_effect_state_check CHECK (((effect_state)::text = ANY (ARRAY[('possible'::character varying)::text, ('confirmed'::character varying)::text]))),
    CONSTRAINT workflow_task_effect_t_request_digest_check CHECK (((request_digest)::text ~ '^sha256:[0-9a-f]{64}$'::text))
);

CREATE TABLE workflow_ops.workflow_tool_access_request_item_t (
    host_id uuid NOT NULL,
    request_id uuid NOT NULL,
    tool_id uuid NOT NULL,
    capability_ref character varying(512) NOT NULL,
    tool_version character varying(20) NOT NULL,
    lightapi_digest character varying(71) NOT NULL,
    allowed_environments text[] NOT NULL,
    usage_locations jsonb DEFAULT '[]'::jsonb NOT NULL,
    status character varying(32) NOT NULL,
    CONSTRAINT workflow_tool_access_request_item_t_allowed_environments_check CHECK (((cardinality(allowed_environments) >= 1) AND (cardinality(allowed_environments) <= 16))),
    CONSTRAINT workflow_tool_access_request_item_t_lightapi_digest_check CHECK (((lightapi_digest)::text ~ '^sha256:[0-9a-f]{64}$'::text)),
    CONSTRAINT workflow_tool_access_request_item_t_status_check CHECK (((status)::text = ANY (ARRAY[('REQUESTED'::character varying)::text, ('GRANTED'::character varying)::text, ('REJECTED'::character varying)::text, ('STALE'::character varying)::text, ('CANCELLED'::character varying)::text, ('FAILED'::character varying)::text]))),
    CONSTRAINT workflow_tool_access_request_item_t_usage_locations_check CHECK ((jsonb_typeof(usage_locations) = 'array'::text))
);

CREATE TABLE workflow_ops.workflow_tool_access_request_t (
    host_id uuid NOT NULL,
    request_id uuid NOT NULL,
    target_wf_def_id uuid NOT NULL,
    requester_user_id uuid NOT NULL,
    approval_wf_def_id uuid NOT NULL,
    approval_wf_instance_id character varying(126) NOT NULL,
    approval_definition_digest character varying(71) NOT NULL,
    request_digest character varying(71) NOT NULL,
    justification character varying(2000) NOT NULL,
    status character varying(32) NOT NULL,
    decision_user_id uuid,
    decision_comment character varying(2000),
    requested_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    decided_ts timestamp with time zone,
    error_code character varying(64),
    error_message character varying(2000),
    aggregate_version bigint DEFAULT 1 NOT NULL,
    CONSTRAINT workflow_tool_access_request_t_aggregate_version_check CHECK ((aggregate_version > 0)),
    CONSTRAINT workflow_tool_access_request_t_approval_definition_digest_check CHECK (((approval_definition_digest)::text ~ '^sha256:[0-9a-f]{64}$'::text)),
    CONSTRAINT workflow_tool_access_request_t_check CHECK (((((status)::text = 'REQUESTED'::text) AND (decided_ts IS NULL)) OR (((status)::text <> 'REQUESTED'::text) AND (decided_ts IS NOT NULL)))),
    CONSTRAINT workflow_tool_access_request_t_check1 CHECK ((((status)::text <> ALL (ARRAY[('STALE'::character varying)::text, ('FAILED'::character varying)::text])) OR (error_code IS NOT NULL))),
    CONSTRAINT workflow_tool_access_request_t_justification_check CHECK ((length(TRIM(BOTH FROM justification)) > 0)),
    CONSTRAINT workflow_tool_access_request_t_request_digest_check CHECK (((request_digest)::text ~ '^sha256:[0-9a-f]{64}$'::text)),
    CONSTRAINT workflow_tool_access_request_t_status_check CHECK (((status)::text = ANY (ARRAY[('REQUESTED'::character varying)::text, ('GRANTED'::character varying)::text, ('REJECTED'::character varying)::text, ('STALE'::character varying)::text, ('CANCELLED'::character varying)::text, ('FAILED'::character varying)::text])))
);

CREATE TABLE workflow_ops.workflow_tool_approval_evidence_t (
    host_id uuid NOT NULL,
    binding_id uuid NOT NULL,
    task_name character varying(255) NOT NULL,
    evidence_digest character varying(71) NOT NULL,
    approved_by character varying(255) NOT NULL,
    approved_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    active boolean DEFAULT true NOT NULL,
    CONSTRAINT workflow_tool_approval_evidence_t_evidence_digest_check CHECK (((evidence_digest)::text ~ '^sha256:[0-9a-f]{64}$'::text))
);

CREATE TABLE workflow_ops.wf_definition_t (
    host_id uuid NOT NULL,
    wf_def_id uuid NOT NULL,
    namespace character varying(126) NOT NULL,
    name character varying(126) NOT NULL,
    version character varying(20) NOT NULL,
    definition text NOT NULL,
    lifecycle_status character varying(16) DEFAULT 'DRAFT'::character varying NOT NULL,
    catalog_visible boolean,
    owner_user_id uuid,
    owner_position_id character varying(128),
    aggregate_version bigint DEFAULT 1 NOT NULL,
    active boolean DEFAULT true,
    update_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP,
    update_user character varying(126) DEFAULT SESSION_USER,
    CONSTRAINT wf_definition_t_lifecycle_status_check CHECK (((lifecycle_status)::text = ANY (ARRAY[('DRAFT'::character varying)::text, ('PUBLISHED'::character varying)::text, ('DEPRECATED'::character varying)::text])))
);

CREATE TABLE workflow_ops.workflow_endpoint_target_t (
    host_id uuid NOT NULL,
    binding_id uuid NOT NULL,
    endpoint_ref character varying(255) NOT NULL,
    endpoint_uri text NOT NULL,
    allowed_methods text[] NOT NULL,
    authorization_policy_digest character varying(71) NOT NULL,
    active boolean DEFAULT true NOT NULL,
    update_user character varying(126) DEFAULT SESSION_USER NOT NULL,
    update_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    CONSTRAINT workflow_endpoint_target_t_allowed_methods_check CHECK ((cardinality(allowed_methods) > 0)),
    CONSTRAINT workflow_endpoint_target_t_allowed_methods_check1 CHECK ((allowed_methods <@ ARRAY['GET'::text, 'HEAD'::text, 'POST'::text, 'PUT'::text, 'PATCH'::text, 'DELETE'::text])),
    CONSTRAINT workflow_endpoint_target_t_authorization_policy_digest_check CHECK (((authorization_policy_digest)::text ~ '^sha256:[0-9a-f]{64}$'::text)),
    CONSTRAINT workflow_endpoint_target_t_endpoint_uri_check CHECK ((endpoint_uri ~ '^https?://'::text))
);

CREATE TABLE workflow_ops.workflow_execution_policy_t (
    policy_snapshot_id uuid NOT NULL,
    host_id uuid NOT NULL,
    tenant_id uuid,
    definition_digest character varying(64) NOT NULL,
    profile_id character varying(126) NOT NULL,
    profile_version integer NOT NULL,
    resolved_policy jsonb NOT NULL,
    policy_digest character varying(64) NOT NULL,
    source character varying(126) NOT NULL,
    created_by character varying(126) NOT NULL,
    created_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL
);

CREATE TABLE workflow_ops.workflow_tool_binding_t (
    host_id uuid NOT NULL,
    binding_id uuid NOT NULL,
    tool_id uuid NOT NULL,
    wf_def_id uuid NOT NULL,
    workflow_version character varying(64) NOT NULL,
    definition_digest character varying(71) NOT NULL,
    schema_digest character varying(71) NOT NULL,
    invocation_mode character varying(8) NOT NULL,
    sync_wait_ms integer NOT NULL,
    total_deadline_ms integer NOT NULL,
    execution_class character varying(16) NOT NULL,
    result_text_mode character varying(16) NOT NULL,
    idempotency_policy jsonb NOT NULL,
    delegation_policy jsonb NOT NULL,
    response_policy_digest character varying(71) NOT NULL,
    runtime_bounds jsonb NOT NULL,
    aggregate_version bigint DEFAULT 1 NOT NULL,
    active boolean DEFAULT true NOT NULL,
    update_user character varying(126) DEFAULT SESSION_USER NOT NULL,
    update_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    policy_digest character varying(71) NOT NULL,
    CONSTRAINT workflow_tool_binding_t_aggregate_version_check CHECK ((aggregate_version > 0)),
    CONSTRAINT workflow_tool_binding_t_check CHECK ((total_deadline_ms >= sync_wait_ms)),
    CONSTRAINT workflow_tool_binding_t_check1 CHECK ((((invocation_mode)::text <> 'sync'::text) OR ((execution_class)::text = 'interactive'::text))),
    CONSTRAINT workflow_tool_binding_t_definition_digest_check CHECK (((definition_digest)::text ~ '^sha256:[0-9a-f]{64}$'::text)),
    CONSTRAINT workflow_tool_binding_t_delegation_policy_check CHECK ((jsonb_typeof(delegation_policy) = 'object'::text)),
    CONSTRAINT workflow_tool_binding_t_execution_class_check CHECK (((execution_class)::text = ANY (ARRAY[('interactive'::character varying)::text, ('standard'::character varying)::text, ('batch'::character varying)::text]))),
    CONSTRAINT workflow_tool_binding_t_idempotency_policy_check CHECK ((jsonb_typeof(idempotency_policy) = 'object'::text)),
    CONSTRAINT workflow_tool_binding_t_invocation_mode_check CHECK (((invocation_mode)::text = ANY (ARRAY[('sync'::character varying)::text, ('async'::character varying)::text]))),
    CONSTRAINT workflow_tool_binding_t_policy_digest_check CHECK (((policy_digest)::text ~ '^sha256:[0-9a-f]{64}$'::text)),
    CONSTRAINT workflow_tool_binding_t_response_policy_digest_check CHECK (((response_policy_digest)::text ~ '^sha256:[0-9a-f]{64}$'::text)),
    CONSTRAINT workflow_tool_binding_t_result_text_mode_check CHECK (((result_text_mode)::text = ANY (ARRAY[('compact-json'::character varying)::text, ('summary'::character varying)::text]))),
    CONSTRAINT workflow_tool_binding_t_runtime_bounds_check CHECK ((jsonb_typeof(runtime_bounds) = 'object'::text)),
    CONSTRAINT workflow_tool_binding_t_schema_digest_check CHECK (((schema_digest)::text ~ '^sha256:[0-9a-f]{64}$'::text)),
    CONSTRAINT workflow_tool_binding_t_sync_wait_ms_check CHECK (((sync_wait_ms >= 1) AND (sync_wait_ms <= 120000)))
);

CREATE TABLE workflow_ops.workflow_tool_dependency_t (
    host_id uuid NOT NULL,
    outer_binding_id uuid NOT NULL,
    nested_tool_id uuid NOT NULL,
    nested_tool_version character varying(64) NOT NULL,
    contract_digest character varying(71) NOT NULL,
    compatibility_policy character varying(32) NOT NULL,
    authorization_tool_name character varying(126) NOT NULL,
    authorization_endpoint_key character varying(255) NOT NULL,
    authorization_policy_digest character varying(71) NOT NULL,
    lifecycle_status character varying(16) NOT NULL,
    dispatch_target jsonb NOT NULL,
    retention_until timestamp with time zone,
    active boolean DEFAULT true NOT NULL,
    update_user character varying(126) DEFAULT SESSION_USER NOT NULL,
    update_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    CONSTRAINT workflow_tool_dependency_t_authorization_policy_digest_check CHECK (((authorization_policy_digest)::text ~ '^sha256:[0-9a-f]{64}$'::text)),
    CONSTRAINT workflow_tool_dependency_t_compatibility_policy_check CHECK (((compatibility_policy)::text = ANY (ARRAY[('exact'::character varying)::text, ('follow-compatible'::character varying)::text]))),
    CONSTRAINT workflow_tool_dependency_t_contract_digest_check CHECK (((contract_digest)::text ~ '^sha256:[0-9a-f]{64}$'::text)),
    CONSTRAINT workflow_tool_dependency_t_dispatch_target_check CHECK ((jsonb_typeof(dispatch_target) = 'object'::text)),
    CONSTRAINT workflow_tool_dependency_t_lifecycle_status_check CHECK (((lifecycle_status)::text = ANY (ARRAY[('active'::character varying)::text, ('superseded'::character varying)::text, ('retirement-candidate'::character varying)::text, ('revoked'::character varying)::text])))
);

CREATE TABLE workflow_ops.workflow_tool_grant_t (
    host_id uuid NOT NULL,
    grant_id uuid NOT NULL,
    tool_id uuid NOT NULL,
    wf_def_id uuid NOT NULL,
    tool_version character varying(20) NOT NULL,
    lightapi_digest character varying(71) NOT NULL,
    allowed_environments text[] NOT NULL,
    aggregate_version bigint DEFAULT 1 NOT NULL,
    active boolean DEFAULT true NOT NULL,
    update_user character varying(126) DEFAULT SESSION_USER NOT NULL,
    update_ts timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    CONSTRAINT workflow_tool_grant_t_aggregate_version_check CHECK ((aggregate_version > 0)),
    CONSTRAINT workflow_tool_grant_t_allowed_environments_check CHECK ((cardinality(allowed_environments) > 0)),
    CONSTRAINT workflow_tool_grant_t_lightapi_digest_check CHECK (((lightapi_digest)::text ~ '^sha256:[0-9a-f]{64}$'::text))
);

ALTER TABLE ONLY workflow_ops.process_info_t
    ADD CONSTRAINT process_info_t_pkey PRIMARY KEY (host_id, process_id);

ALTER TABLE ONLY workflow_ops.task_info_t
    ADD CONSTRAINT task_info_t_pkey PRIMARY KEY (host_id, task_id);

ALTER TABLE ONLY workflow_ops.wf_definition_t
    ADD CONSTRAINT wf_definition_t_host_id_namespace_name_version_key UNIQUE (host_id, namespace, name, version);

ALTER TABLE ONLY workflow_ops.wf_definition_t
    ADD CONSTRAINT wf_definition_t_pkey PRIMARY KEY (host_id, wf_def_id);

ALTER TABLE ONLY workflow_ops.workflow_approval_t
    ADD CONSTRAINT workflow_approval_t_pkey PRIMARY KEY (host_id, approval_id);

ALTER TABLE ONLY workflow_ops.workflow_artifact_t
    ADD CONSTRAINT workflow_artifact_t_pkey PRIMARY KEY (host_id, artifact_id);

ALTER TABLE ONLY workflow_ops.workflow_endpoint_target_t
    ADD CONSTRAINT workflow_endpoint_target_t_pkey PRIMARY KEY (host_id, endpoint_ref);

ALTER TABLE ONLY workflow_ops.workflow_execution_policy_t
    ADD CONSTRAINT workflow_execution_policy_t_host_id_policy_digest_key UNIQUE (host_id, policy_digest);

ALTER TABLE ONLY workflow_ops.workflow_execution_policy_t
    ADD CONSTRAINT workflow_execution_policy_t_host_id_policy_snapshot_id_key UNIQUE (host_id, policy_snapshot_id);

ALTER TABLE ONLY workflow_ops.workflow_execution_policy_t
    ADD CONSTRAINT workflow_execution_policy_t_pkey PRIMARY KEY (policy_snapshot_id);

ALTER TABLE ONLY workflow_ops.workflow_executor_tenant_turn_t
    ADD CONSTRAINT workflow_executor_tenant_turn_t_pkey PRIMARY KEY (host_id);

ALTER TABLE ONLY workflow_ops.workflow_fork_branch_t
    ADD CONSTRAINT workflow_fork_branch_t_host_id_task_id_key UNIQUE (host_id, task_id);

ALTER TABLE ONLY workflow_ops.workflow_fork_branch_t
    ADD CONSTRAINT workflow_fork_branch_t_pkey PRIMARY KEY (host_id, join_id, branch_name);

ALTER TABLE ONLY workflow_ops.workflow_fork_join_t
    ADD CONSTRAINT workflow_fork_join_t_host_id_fork_task_id_key UNIQUE (host_id, fork_task_id);

ALTER TABLE ONLY workflow_ops.workflow_fork_join_t
    ADD CONSTRAINT workflow_fork_join_t_pkey PRIMARY KEY (host_id, join_id);

ALTER TABLE ONLY workflow_ops.workflow_invocation_audit_outbox_t
    ADD CONSTRAINT workflow_invocation_audit_out_host_id_workflow_instance_id__key UNIQUE (host_id, workflow_instance_id, event_type);

ALTER TABLE ONLY workflow_ops.workflow_invocation_audit_outbox_t
    ADD CONSTRAINT workflow_invocation_audit_outbox_t_pkey PRIMARY KEY (host_id, event_id);

ALTER TABLE ONLY workflow_ops.workflow_invocation_budget_reservation_t
    ADD CONSTRAINT workflow_invocation_budget_reservation_t_pkey PRIMARY KEY (host_id, reservation_id);

ALTER TABLE ONLY workflow_ops.workflow_invocation_budget_t
    ADD CONSTRAINT workflow_invocation_budget_t_host_id_workflow_instance_id_key UNIQUE (host_id, workflow_instance_id);

ALTER TABLE ONLY workflow_ops.workflow_invocation_budget_t
    ADD CONSTRAINT workflow_invocation_budget_t_pkey PRIMARY KEY (host_id, ledger_id);

ALTER TABLE ONLY workflow_ops.workflow_invocation_event_quarantine_t
    ADD CONSTRAINT workflow_invocation_event_qua_consumer_group_partition_id_s_key UNIQUE (consumer_group, partition_id, source_offset);

ALTER TABLE ONLY workflow_ops.workflow_invocation_event_quarantine_t
    ADD CONSTRAINT workflow_invocation_event_quarantine_t_pkey PRIMARY KEY (host_id, quarantine_id);

ALTER TABLE ONLY workflow_ops.workflow_invocation_idempotency_t
    ADD CONSTRAINT workflow_invocation_idempotency_t_pkey PRIMARY KEY (host_id, reservation_id);

ALTER TABLE ONLY workflow_ops.workflow_invocation_t
    ADD CONSTRAINT workflow_invocation_t_host_id_process_id_key UNIQUE (host_id, process_id);

ALTER TABLE ONLY workflow_ops.workflow_invocation_t
    ADD CONSTRAINT workflow_invocation_t_pkey PRIMARY KEY (host_id, workflow_instance_id);

ALTER TABLE ONLY workflow_ops.workflow_task_effect_t
    ADD CONSTRAINT workflow_task_effect_t_pkey PRIMARY KEY (host_id, workflow_instance_id, task_name, idempotency_key);

ALTER TABLE ONLY workflow_ops.workflow_tool_access_request_t
    ADD CONSTRAINT workflow_tool_access_request__host_id_approval_wf_instance__key UNIQUE (host_id, approval_wf_instance_id);

ALTER TABLE ONLY workflow_ops.workflow_tool_access_request_item_t
    ADD CONSTRAINT workflow_tool_access_request__host_id_request_id_capability_key UNIQUE (host_id, request_id, capability_ref);

ALTER TABLE ONLY workflow_ops.workflow_tool_access_request_item_t
    ADD CONSTRAINT workflow_tool_access_request_item_t_pkey PRIMARY KEY (host_id, request_id, tool_id);

ALTER TABLE ONLY workflow_ops.workflow_tool_access_request_t
    ADD CONSTRAINT workflow_tool_access_request_t_pkey PRIMARY KEY (host_id, request_id);

ALTER TABLE ONLY workflow_ops.workflow_tool_approval_evidence_t
    ADD CONSTRAINT workflow_tool_approval_evidence_t_pkey PRIMARY KEY (host_id, binding_id, task_name, evidence_digest);

ALTER TABLE ONLY workflow_ops.workflow_tool_binding_t
    ADD CONSTRAINT workflow_tool_binding_skill_target_uq UNIQUE (host_id, binding_id, wf_def_id, tool_id);

ALTER TABLE ONLY workflow_ops.workflow_tool_binding_t
    ADD CONSTRAINT workflow_tool_binding_t_host_id_tool_id_workflow_version_key UNIQUE (host_id, tool_id, workflow_version);

ALTER TABLE ONLY workflow_ops.workflow_tool_binding_t
    ADD CONSTRAINT workflow_tool_binding_t_pkey PRIMARY KEY (host_id, binding_id);

ALTER TABLE ONLY workflow_ops.workflow_tool_dependency_t
    ADD CONSTRAINT workflow_tool_dependency_t_pkey PRIMARY KEY (host_id, outer_binding_id, nested_tool_id, nested_tool_version);

ALTER TABLE ONLY workflow_ops.workflow_tool_grant_t
    ADD CONSTRAINT workflow_tool_grant_t_pkey PRIMARY KEY (host_id, grant_id);

ALTER TABLE ONLY workflow_ops.process_info_t
    ADD CONSTRAINT process_info_policy_snapshot_fk FOREIGN KEY (host_id, policy_snapshot_id) REFERENCES workflow_ops.workflow_execution_policy_t(host_id, policy_snapshot_id) ON DELETE RESTRICT;

ALTER TABLE ONLY workflow_ops.process_info_t
    ADD CONSTRAINT process_info_t_host_id_wf_def_id_fkey FOREIGN KEY (host_id, wf_def_id) REFERENCES workflow_ops.wf_definition_t(host_id, wf_def_id) ON DELETE CASCADE;

ALTER TABLE ONLY workflow_ops.task_info_t
    ADD CONSTRAINT task_info_t_host_id_process_id_fkey FOREIGN KEY (host_id, process_id) REFERENCES workflow_ops.process_info_t(host_id, process_id) ON DELETE CASCADE;

ALTER TABLE ONLY workflow_ops.workflow_approval_t
    ADD CONSTRAINT workflow_approval_t_host_id_process_id_fkey FOREIGN KEY (host_id, process_id) REFERENCES workflow_ops.process_info_t(host_id, process_id) ON DELETE RESTRICT;

ALTER TABLE ONLY workflow_ops.workflow_approval_t
    ADD CONSTRAINT workflow_approval_t_host_id_task_id_fkey FOREIGN KEY (host_id, task_id) REFERENCES workflow_ops.task_info_t(host_id, task_id) ON DELETE RESTRICT;

ALTER TABLE ONLY workflow_ops.workflow_endpoint_target_t
    ADD CONSTRAINT workflow_endpoint_target_t_host_id_binding_id_fkey FOREIGN KEY (host_id, binding_id) REFERENCES workflow_ops.workflow_tool_binding_t(host_id, binding_id) ON DELETE CASCADE;

ALTER TABLE ONLY workflow_ops.workflow_fork_branch_t
    ADD CONSTRAINT workflow_fork_branch_t_host_id_join_id_fkey FOREIGN KEY (host_id, join_id) REFERENCES workflow_ops.workflow_fork_join_t(host_id, join_id) ON DELETE RESTRICT;

ALTER TABLE ONLY workflow_ops.workflow_fork_branch_t
    ADD CONSTRAINT workflow_fork_branch_t_host_id_task_id_fkey FOREIGN KEY (host_id, task_id) REFERENCES workflow_ops.task_info_t(host_id, task_id) ON DELETE RESTRICT;

ALTER TABLE ONLY workflow_ops.workflow_fork_join_t
    ADD CONSTRAINT workflow_fork_join_t_host_id_fork_task_id_fkey FOREIGN KEY (host_id, fork_task_id) REFERENCES workflow_ops.task_info_t(host_id, task_id) ON DELETE RESTRICT;

ALTER TABLE ONLY workflow_ops.workflow_fork_join_t
    ADD CONSTRAINT workflow_fork_join_t_host_id_process_id_fkey FOREIGN KEY (host_id, process_id) REFERENCES workflow_ops.process_info_t(host_id, process_id) ON DELETE RESTRICT;

ALTER TABLE ONLY workflow_ops.workflow_invocation_audit_outbox_t
    ADD CONSTRAINT workflow_invocation_audit_out_host_id_workflow_instance_id_fkey FOREIGN KEY (host_id, workflow_instance_id) REFERENCES workflow_ops.workflow_invocation_t(host_id, workflow_instance_id) ON DELETE RESTRICT;

ALTER TABLE ONLY workflow_ops.workflow_invocation_budget_reservation_t
    ADD CONSTRAINT workflow_invocation_budget_reservation_t_host_id_ledger_id_fkey FOREIGN KEY (host_id, ledger_id) REFERENCES workflow_ops.workflow_invocation_budget_t(host_id, ledger_id) ON DELETE RESTRICT;

ALTER TABLE ONLY workflow_ops.workflow_invocation_budget_t
    ADD CONSTRAINT workflow_invocation_budget_t_host_id_workflow_instance_id_fkey FOREIGN KEY (host_id, workflow_instance_id) REFERENCES workflow_ops.workflow_invocation_t(host_id, workflow_instance_id) ON DELETE RESTRICT;

ALTER TABLE ONLY workflow_ops.workflow_invocation_t
    ADD CONSTRAINT workflow_invocation_t_host_id_binding_id_fkey FOREIGN KEY (host_id, binding_id) REFERENCES workflow_ops.workflow_tool_binding_t(host_id, binding_id) ON DELETE RESTRICT;

ALTER TABLE ONLY workflow_ops.workflow_invocation_t
    ADD CONSTRAINT workflow_invocation_t_host_id_process_id_fkey FOREIGN KEY (host_id, process_id) REFERENCES workflow_ops.process_info_t(host_id, process_id) ON DELETE RESTRICT;

ALTER TABLE ONLY workflow_ops.workflow_invocation_t
    ADD CONSTRAINT workflow_invocation_t_host_id_wf_def_id_fkey FOREIGN KEY (host_id, wf_def_id) REFERENCES workflow_ops.wf_definition_t(host_id, wf_def_id) ON DELETE RESTRICT;

ALTER TABLE ONLY workflow_ops.workflow_task_effect_t
    ADD CONSTRAINT workflow_task_effect_t_host_id_workflow_instance_id_fkey FOREIGN KEY (host_id, workflow_instance_id) REFERENCES workflow_ops.workflow_invocation_t(host_id, workflow_instance_id) ON DELETE RESTRICT;

ALTER TABLE ONLY workflow_ops.workflow_tool_access_request_item_t
    ADD CONSTRAINT workflow_tool_access_request_item_t_host_id_request_id_fkey FOREIGN KEY (host_id, request_id) REFERENCES workflow_ops.workflow_tool_access_request_t(host_id, request_id) ON DELETE CASCADE;

ALTER TABLE ONLY workflow_ops.workflow_tool_access_request_t
    ADD CONSTRAINT workflow_tool_access_request_t_host_id_approval_wf_def_id_fkey FOREIGN KEY (host_id, approval_wf_def_id) REFERENCES workflow_ops.wf_definition_t(host_id, wf_def_id) ON DELETE RESTRICT;

ALTER TABLE ONLY workflow_ops.workflow_tool_access_request_t
    ADD CONSTRAINT workflow_tool_access_request_t_host_id_target_wf_def_id_fkey FOREIGN KEY (host_id, target_wf_def_id) REFERENCES workflow_ops.wf_definition_t(host_id, wf_def_id) ON DELETE RESTRICT;

ALTER TABLE ONLY workflow_ops.workflow_tool_approval_evidence_t
    ADD CONSTRAINT workflow_tool_approval_evidence_t_host_id_binding_id_fkey FOREIGN KEY (host_id, binding_id) REFERENCES workflow_ops.workflow_tool_binding_t(host_id, binding_id) ON DELETE RESTRICT;

ALTER TABLE ONLY workflow_ops.workflow_tool_binding_t
    ADD CONSTRAINT workflow_tool_binding_t_host_id_wf_def_id_fkey FOREIGN KEY (host_id, wf_def_id) REFERENCES workflow_ops.wf_definition_t(host_id, wf_def_id) ON DELETE RESTRICT;

ALTER TABLE ONLY workflow_ops.workflow_tool_dependency_t
    ADD CONSTRAINT workflow_tool_dependency_t_host_id_outer_binding_id_fkey FOREIGN KEY (host_id, outer_binding_id) REFERENCES workflow_ops.workflow_tool_binding_t(host_id, binding_id) ON DELETE RESTRICT;

ALTER TABLE ONLY workflow_ops.workflow_tool_grant_t
    ADD CONSTRAINT workflow_tool_grant_t_host_id_wf_def_id_fkey FOREIGN KEY (host_id, wf_def_id) REFERENCES workflow_ops.wf_definition_t(host_id, wf_def_id) ON DELETE RESTRICT;

CREATE INDEX idx_wf_definition_catalog_visible ON workflow_ops.wf_definition_t USING btree (host_id, catalog_visible) WHERE (catalog_visible = true);

CREATE INDEX idx_wf_definition_owner_position ON workflow_ops.wf_definition_t USING btree (host_id, owner_position_id);

CREATE INDEX idx_wf_definition_owner_user ON workflow_ops.wf_definition_t USING btree (host_id, owner_user_id);

CREATE UNIQUE INDEX process_info_source_event_uk ON workflow_ops.process_info_t USING btree (host_id, wf_def_id, source_event_id) WHERE (source_event_id IS NOT NULL);

CREATE INDEX task_info_active_host_idx ON workflow_ops.task_info_t USING btree (host_id, priority DESC, started_ts, task_id) WHERE ((active = true) AND (status_code = 'A'::bpchar) AND ((execution_placement)::text = 'host'::text));

CREATE INDEX task_info_active_runner_idx ON workflow_ops.task_info_t USING btree (host_id, priority DESC, started_ts, task_id) WHERE ((active = true) AND (status_code = 'A'::bpchar) AND ((execution_placement)::text = 'runner'::text));

CREATE INDEX task_info_host_fair_claim_v1_idx ON workflow_ops.task_info_t USING btree (execution_class, priority DESC, started_ts, host_id, task_id) WHERE (active AND ((execution_placement)::text = 'host'::text) AND (status_code = ANY (ARRAY['A'::bpchar, 'C'::bpchar])));

CREATE INDEX task_info_phase2_claim_idx ON workflow_ops.task_info_t USING btree (execution_class, next_attempt_ts, priority DESC, started_ts, host_id) WHERE (active AND ((execution_placement)::text = 'host'::text) AND (status_code = ANY (ARRAY['A'::bpchar, 'C'::bpchar])));

CREATE UNIQUE INDEX workflow_approval_active_uk ON workflow_ops.workflow_approval_t USING btree (host_id, process_id, task_id, policy_digest, target, operation) WHERE ((state)::text = ANY (ARRAY[('REQUESTED'::character varying)::text, ('APPROVED'::character varying)::text]));

CREATE INDEX workflow_artifact_retention_idx ON workflow_ops.workflow_artifact_t USING btree (deletion_state, legal_hold, retain_until_ts, deletion_next_retry_ts) WHERE ((deletion_state)::text = ANY (ARRAY[('RETAINED'::character varying)::text, ('DELETE_PENDING'::character varying)::text, ('DELETE_FAILED'::character varying)::text]));

CREATE INDEX workflow_invocation_audit_pending_idx ON workflow_ops.workflow_invocation_audit_outbox_t USING btree (event_ts, event_id) WHERE (published_ts IS NULL);

CREATE UNIQUE INDEX workflow_invocation_idempotency_current_uq ON workflow_ops.workflow_invocation_idempotency_t USING btree (host_id, scope_digest) WHERE active;

CREATE INDEX workflow_invocation_quarantine_aggregate_idx ON workflow_ops.workflow_invocation_event_quarantine_t USING btree (host_id, aggregate_id, aggregate_version);

CREATE INDEX workflow_invocation_state_idx ON workflow_ops.workflow_invocation_t USING btree (host_id, execution_class, state, deadline_ts);

CREATE INDEX workflow_invocation_subject_idx ON workflow_ops.workflow_invocation_t USING btree (host_id, principal_subject, end_user_subject, accepted_ts DESC);

CREATE INDEX workflow_tool_access_request_requester_idx ON workflow_ops.workflow_tool_access_request_t USING btree (host_id, requester_user_id, status);

CREATE INDEX workflow_tool_access_request_target_idx ON workflow_ops.workflow_tool_access_request_t USING btree (host_id, target_wf_def_id, status);

CREATE UNIQUE INDEX workflow_tool_binding_active_tool_uq ON workflow_ops.workflow_tool_binding_t USING btree (host_id, tool_id) WHERE active;

CREATE INDEX workflow_tool_dependency_reverse_idx ON workflow_ops.workflow_tool_dependency_t USING btree (host_id, nested_tool_id, active, outer_binding_id);

CREATE UNIQUE INDEX workflow_tool_grant_active_scope_uq ON workflow_ops.workflow_tool_grant_t USING btree (host_id, tool_id, wf_def_id) WHERE active;

CREATE INDEX workflow_tool_grant_callable_idx ON workflow_ops.workflow_tool_grant_t USING btree (host_id, wf_def_id, active, tool_id);

CREATE FUNCTION workflow_ops.workflow_claim_host_task_v1(p_worker_id uuid, p_lease_ms integer) RETURNS TABLE(host_id uuid, task_id uuid, task_type character varying, process_id uuid, wf_instance_id character varying, wf_task_id character varying, status_code character, result_code character varying, lease_owner uuid, lease_fencing_token bigint, lease_expires_ts timestamp with time zone)
    LANGUAGE plpgsql
    AS $$
DECLARE claimed_host UUID;
BEGIN
    IF p_lease_ms<100 OR p_lease_ms>30000 THEN RAISE EXCEPTION 'WORKFLOW_HOST_LEASE_MS_OUT_OF_RANGE'; END IF;
    SELECT candidates.host_id INTO claimed_host FROM (
        SELECT t.host_id,
               MIN(CASE t.execution_class WHEN 'interactive' THEN 0 WHEN 'standard' THEN 1 ELSE 2 END) class_rank,
               MAX(t.priority) maximum_priority,MIN(t.started_ts) oldest_task
          FROM task_info_t t
         WHERE t.active AND t.execution_placement='host'
           AND ((t.status_code='A' AND t.task_type IN ('ask','assert','call','set','switch','fork'))
             OR (t.status_code='C' AND t.task_type='ask' AND t.completed_ts IS NOT NULL
                 AND (t.task_output IS NULL OR t.task_output->>'status'='waiting_for_input')))
           AND t.next_attempt_ts<=CURRENT_TIMESTAMP
           AND (t.effect_state='none' OR t.downstream_idempotency_key IS NOT NULL)
           AND (t.locked='N' OR (t.locked='Y' AND t.lease_expires_ts<=CURRENT_TIMESTAMP))
           AND (t.deadline_ts IS NULL OR t.deadline_ts>CURRENT_TIMESTAMP)
         GROUP BY t.host_id
    ) candidates LEFT JOIN workflow_executor_tenant_turn_t turn ON turn.host_id=candidates.host_id
    ORDER BY candidates.class_rank,COALESCE(turn.last_claim_ts,'-infinity'::timestamptz),
             candidates.maximum_priority DESC,candidates.oldest_task,candidates.host_id LIMIT 1;
    IF claimed_host IS NULL THEN RETURN; END IF;
    IF NOT pg_try_advisory_xact_lock(hashtext(claimed_host::text)) THEN RETURN; END IF;
    INSERT INTO workflow_executor_tenant_turn_t(host_id,last_claim_ts,claim_count)
    VALUES(claimed_host,CURRENT_TIMESTAMP,1)
    ON CONFLICT ON CONSTRAINT workflow_executor_tenant_turn_t_pkey DO UPDATE SET
      last_claim_ts=EXCLUDED.last_claim_ts,claim_count=workflow_executor_tenant_turn_t.claim_count+1,
      updated_ts=CURRENT_TIMESTAMP;
    RETURN QUERY WITH candidate AS (
      SELECT t.host_id,t.task_id FROM task_info_t t
       WHERE t.host_id=claimed_host AND t.active AND t.execution_placement='host'
         AND ((t.status_code='A' AND t.task_type IN ('ask','assert','call','set','switch','fork'))
           OR (t.status_code='C' AND t.task_type='ask' AND t.completed_ts IS NOT NULL
               AND (t.task_output IS NULL OR t.task_output->>'status'='waiting_for_input')))
         AND t.next_attempt_ts<=CURRENT_TIMESTAMP
         AND (t.effect_state='none' OR t.downstream_idempotency_key IS NOT NULL)
         AND (t.locked='N' OR (t.locked='Y' AND t.lease_expires_ts<=CURRENT_TIMESTAMP))
         AND (t.deadline_ts IS NULL OR t.deadline_ts>CURRENT_TIMESTAMP)
       ORDER BY CASE t.execution_class WHEN 'interactive' THEN 0 WHEN 'standard' THEN 1 ELSE 2 END,
                t.priority DESC,t.started_ts,t.task_id LIMIT 1 FOR UPDATE SKIP LOCKED
    ) UPDATE task_info_t t SET locked='Y',lease_owner=p_worker_id,
      lease_fencing_token=t.lease_fencing_token+1,
      lease_expires_ts=LEAST(COALESCE(t.deadline_ts,'infinity'::timestamptz),
        CURRENT_TIMESTAMP+make_interval(secs=>p_lease_ms::double precision/1000.0)),update_ts=CURRENT_TIMESTAMP
      FROM candidate c WHERE t.host_id=c.host_id AND t.task_id=c.task_id
    RETURNING t.host_id,t.task_id,t.task_type,t.process_id,t.wf_instance_id,t.wf_task_id,
              t.status_code,t.result_code,t.lease_owner,t.lease_fencing_token,t.lease_expires_ts;
END
$$;

CREATE FUNCTION workflow_ops.workflow_claim_idempotency_v1(p_host_id uuid, p_reservation_id uuid, p_scope_digest character varying, p_idempotency_kind character varying, p_stable_tool_ref uuid, p_principal_subject character varying, p_end_user_subject character varying, p_workflow_instance_id uuid, p_definition_digest character varying, p_input_digest character varying, p_in_flight_until timestamp with time zone, p_result_replay_until timestamp with time zone) RETURNS TABLE(outcome character varying, accepted_workflow_instance_id uuid, accepted_generation bigint)
    LANGUAGE plpgsql
    AS $$
DECLARE current_row workflow_invocation_idempotency_t%ROWTYPE;
BEGIN
    INSERT INTO workflow_invocation_idempotency_t(
        host_id,reservation_id,scope_digest,idempotency_kind,stable_tool_ref,
        principal_subject,end_user_subject,workflow_instance_id,
        definition_digest,input_digest,in_flight_until,result_replay_until
    ) VALUES(
        p_host_id,p_reservation_id,p_scope_digest,p_idempotency_kind,
        p_stable_tool_ref,p_principal_subject,p_end_user_subject,
        p_workflow_instance_id,p_definition_digest,p_input_digest,
        p_in_flight_until,p_result_replay_until
    ) ON CONFLICT(host_id,scope_digest) WHERE active DO NOTHING;
    IF FOUND THEN
        RETURN QUERY SELECT 'ACCEPTED'::VARCHAR,p_workflow_instance_id,1::BIGINT;
        RETURN;
    END IF;
    SELECT * INTO current_row FROM workflow_invocation_idempotency_t
     WHERE host_id=p_host_id AND scope_digest=p_scope_digest AND active FOR UPDATE;
    IF current_row.stable_tool_ref=p_stable_tool_ref
       AND current_row.principal_subject=p_principal_subject
       AND current_row.end_user_subject=p_end_user_subject
       AND current_row.definition_digest=p_definition_digest
       AND current_row.input_digest=p_input_digest THEN
        IF current_row.result_replay_until>CURRENT_TIMESTAMP THEN
            RETURN QUERY SELECT 'REPLAY'::VARCHAR,current_row.workflow_instance_id,current_row.generation;
            RETURN;
        END IF;
        UPDATE workflow_invocation_idempotency_t SET
            active=FALSE,updated_ts=CURRENT_TIMESTAMP
         WHERE host_id=p_host_id AND reservation_id=current_row.reservation_id;
        INSERT INTO workflow_invocation_idempotency_t(
            host_id,reservation_id,scope_digest,idempotency_kind,stable_tool_ref,
            principal_subject,end_user_subject,workflow_instance_id,
            definition_digest,input_digest,generation,in_flight_until,result_replay_until
        ) VALUES(
            p_host_id,p_reservation_id,p_scope_digest,p_idempotency_kind,
            p_stable_tool_ref,p_principal_subject,p_end_user_subject,
            p_workflow_instance_id,p_definition_digest,p_input_digest,
            current_row.generation+1,p_in_flight_until,p_result_replay_until
        );
        RETURN QUERY SELECT 'ACCEPTED'::VARCHAR,p_workflow_instance_id,current_row.generation+1;
    ELSE
        RETURN QUERY SELECT 'CONFLICT'::VARCHAR,current_row.workflow_instance_id,current_row.generation;
    END IF;
END
$$;

CREATE FUNCTION workflow_ops.workflow_claim_task_effect_v1(p_host_id uuid, p_workflow_instance_id uuid, p_task_name character varying, p_idempotency_key character varying, p_request_digest character varying) RETURNS TABLE(claimed boolean, replayed boolean, result jsonb, effect_state character varying)
    LANGUAGE plpgsql
    AS $$
DECLARE existing workflow_task_effect_t%ROWTYPE;
BEGIN
    INSERT INTO workflow_task_effect_t(
        host_id,workflow_instance_id,task_name,idempotency_key,request_digest)
    VALUES(p_host_id,p_workflow_instance_id,p_task_name,p_idempotency_key,p_request_digest)
    ON CONFLICT DO NOTHING;
    SELECT * INTO existing FROM workflow_task_effect_t
     WHERE host_id=p_host_id AND workflow_instance_id=p_workflow_instance_id
       AND task_name=p_task_name AND idempotency_key=p_idempotency_key FOR UPDATE;
    IF existing.request_digest<>p_request_digest THEN
        RAISE EXCEPTION 'WORKFLOW_TASK_IDEMPOTENCY_CONFLICT';
    END IF;
    RETURN QUERY SELECT existing.confirmed_ts IS NULL,existing.confirmed_ts IS NOT NULL,
                        existing.result,existing.effect_state;
END
$$;

CREATE FUNCTION workflow_ops.workflow_confirm_task_effect_v1(p_host_id uuid, p_workflow_instance_id uuid, p_task_name character varying, p_idempotency_key character varying, p_request_digest character varying, p_result jsonb) RETURNS boolean
    LANGUAGE plpgsql
    AS $$
BEGIN
    UPDATE workflow_task_effect_t SET effect_state='confirmed',result=p_result,
           confirmed_ts=COALESCE(confirmed_ts,CURRENT_TIMESTAMP)
     WHERE host_id=p_host_id AND workflow_instance_id=p_workflow_instance_id
       AND task_name=p_task_name AND idempotency_key=p_idempotency_key
       AND request_digest=p_request_digest;
    RETURN FOUND;
END
$$;

CREATE FUNCTION workflow_ops.workflow_reconcile_budget_v1(p_host_id uuid, p_reservation_id uuid, p_fencing_token bigint, p_actual_bytes bigint, p_actual_cost_units bigint) RETURNS boolean
    LANGUAGE plpgsql
    AS $$
DECLARE reservation workflow_invocation_budget_reservation_t%ROWTYPE;
BEGIN
    SELECT * INTO reservation FROM workflow_invocation_budget_reservation_t
     WHERE host_id=p_host_id AND reservation_id=p_reservation_id FOR UPDATE;
    IF NOT FOUND OR reservation.fencing_token<>p_fencing_token THEN RETURN FALSE; END IF;
    IF reservation.state='RECONCILED' THEN
        RETURN reservation.actual_bytes=p_actual_bytes
           AND reservation.actual_cost_units=p_actual_cost_units;
    END IF;
    IF reservation.state<>'RESERVED' OR p_actual_bytes<0 OR p_actual_cost_units<0
       OR p_actual_bytes>reservation.reserved_bytes
       OR p_actual_cost_units>reservation.reserved_cost_units THEN RETURN FALSE; END IF;
    UPDATE workflow_invocation_budget_t SET
        task_attempt_reserved=task_attempt_reserved-reservation.task_attempts,
        nested_call_reserved=nested_call_reserved-reservation.nested_calls,
        byte_reserved=byte_reserved-reservation.reserved_bytes,
        cost_unit_reserved=cost_unit_reserved-reservation.reserved_cost_units,
        task_attempt_used=task_attempt_used+reservation.task_attempts,
        nested_call_used=nested_call_used+reservation.nested_calls,
        byte_used=byte_used+p_actual_bytes,
        cost_unit_used=cost_unit_used+p_actual_cost_units,
        updated_ts=CURRENT_TIMESTAMP
     WHERE host_id=p_host_id AND ledger_id=reservation.ledger_id
       AND generation=reservation.generation;
    IF NOT FOUND THEN RETURN FALSE; END IF;
    UPDATE workflow_invocation_budget_reservation_t SET
        state='RECONCILED',actual_bytes=p_actual_bytes,
        actual_cost_units=p_actual_cost_units,reconciled_ts=CURRENT_TIMESTAMP
     WHERE host_id=p_host_id AND reservation_id=p_reservation_id;
    RETURN TRUE;
END
$$;

CREATE FUNCTION workflow_ops.workflow_reserve_budget_v1(p_host_id uuid, p_ledger_id uuid, p_reservation_id uuid, p_generation bigint, p_fencing_token bigint, p_task_attempts bigint, p_nested_calls bigint, p_bytes bigint, p_cost_units bigint) RETURNS boolean
    LANGUAGE plpgsql
    AS $$
DECLARE existing workflow_invocation_budget_reservation_t%ROWTYPE;
BEGIN
    IF p_task_attempts < 0 OR p_nested_calls < 0 OR p_bytes < 0 OR p_cost_units < 0
       OR p_fencing_token <= 0 THEN
        RAISE EXCEPTION 'WORKFLOW_BUDGET_INVALID_RESERVATION';
    END IF;
    SELECT * INTO existing FROM workflow_invocation_budget_reservation_t
     WHERE host_id=p_host_id AND reservation_id=p_reservation_id FOR UPDATE;
    IF FOUND THEN
        IF existing.ledger_id=p_ledger_id AND existing.generation=p_generation
           AND existing.fencing_token=p_fencing_token
           AND existing.task_attempts=p_task_attempts
           AND existing.nested_calls=p_nested_calls
           AND existing.reserved_bytes=p_bytes
           AND existing.reserved_cost_units=p_cost_units THEN
            RETURN existing.state IN ('RESERVED','RECONCILED');
        END IF;
        RAISE EXCEPTION 'WORKFLOW_BUDGET_RESERVATION_CONFLICT';
    END IF;
    UPDATE workflow_invocation_budget_t SET
        task_attempt_reserved=task_attempt_reserved+p_task_attempts,
        nested_call_reserved=nested_call_reserved+p_nested_calls,
        byte_reserved=byte_reserved+p_bytes,
        cost_unit_reserved=cost_unit_reserved+p_cost_units,
        updated_ts=CURRENT_TIMESTAMP
     WHERE host_id=p_host_id AND ledger_id=p_ledger_id
       AND generation=p_generation AND deadline_ts>CURRENT_TIMESTAMP
       AND task_attempt_used+task_attempt_reserved+p_task_attempts<=task_attempt_limit
       AND nested_call_used+nested_call_reserved+p_nested_calls<=nested_call_limit
       AND byte_used+byte_reserved+p_bytes<=byte_limit
       AND cost_unit_used+cost_unit_reserved+p_cost_units<=cost_unit_limit;
    IF NOT FOUND THEN RETURN FALSE; END IF;
    INSERT INTO workflow_invocation_budget_reservation_t(
        host_id,reservation_id,ledger_id,generation,fencing_token,
        task_attempts,nested_calls,reserved_bytes,reserved_cost_units,state
    ) VALUES(p_host_id,p_reservation_id,p_ledger_id,p_generation,p_fencing_token,
             p_task_attempts,p_nested_calls,p_bytes,p_cost_units,'RESERVED');
    RETURN TRUE;
END
$$;

GRANT USAGE ON SCHEMA workflow_ops TO operations_workflow_runtime;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA workflow_ops TO operations_workflow_runtime;
GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA workflow_ops TO operations_workflow_runtime;
ALTER DEFAULT PRIVILEGES FOR ROLE operations_workflow_migrator IN SCHEMA workflow_ops
  GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO operations_workflow_runtime;
ALTER DEFAULT PRIVILEGES FOR ROLE operations_workflow_migrator IN SCHEMA workflow_ops
  GRANT EXECUTE ON FUNCTIONS TO operations_workflow_runtime;
