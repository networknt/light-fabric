-- Workflow-owned development stage ledger; never Portal configuration authority.
CREATE TABLE workflow_ops.development_vm_t (
    host_id uuid NOT NULL,
    vm_id text NOT NULL CHECK (length(vm_id) BETWEEN 1 AND 256),
    generation bigint NOT NULL CHECK (generation > 0),
    feature_id text,
    PRIMARY KEY (host_id, vm_id)
);
CREATE TABLE workflow_ops.development_feature_t (
    host_id uuid NOT NULL,
    feature_id text NOT NULL CHECK (length(feature_id) BETWEEN 1 AND 256),
    principal_subject text NOT NULL,
    end_user_subject text NOT NULL,
    vm_id text NOT NULL,
    generation bigint NOT NULL CHECK (generation > 0),
    version bigint NOT NULL CHECK (version > 0),
    creation_digest text NOT NULL,
    record jsonb NOT NULL CHECK (jsonb_typeof(record) = 'object'),
    finding_ledger jsonb,
    created_ts timestamptz NOT NULL DEFAULT clock_timestamp(),
    updated_ts timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (host_id, feature_id),
    FOREIGN KEY (host_id, vm_id) REFERENCES workflow_ops.development_vm_t(host_id, vm_id)
);

CREATE TABLE workflow_ops.development_stage_t (
    host_id uuid NOT NULL,
    claim_id text NOT NULL,
    feature_id text NOT NULL,
    process_id uuid NOT NULL,
    workflow_instance_id uuid NOT NULL,
    invocation_digest text NOT NULL,
    request jsonb NOT NULL,
    receipt jsonb NOT NULL,
    result jsonb,
    signoff jsonb,
    created_ts timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (host_id, claim_id),
    UNIQUE (host_id, process_id),
    UNIQUE (host_id, workflow_instance_id),
    FOREIGN KEY (host_id, feature_id) REFERENCES workflow_ops.development_feature_t(host_id, feature_id),
    FOREIGN KEY (host_id, process_id) REFERENCES workflow_ops.process_info_t(host_id, process_id),
    FOREIGN KEY (host_id, workflow_instance_id) REFERENCES workflow_ops.workflow_invocation_t(host_id, workflow_instance_id)
);

CREATE TABLE workflow_ops.development_turn_t (
    host_id uuid NOT NULL,
    feature_id text NOT NULL,
    logical_turn_id text NOT NULL,
    claim_id text NOT NULL,
    dispatch_token uuid NOT NULL,
    task_id uuid,
    request_digest text NOT NULL,
    charge jsonb NOT NULL,
    result jsonb,
    created_ts timestamptz NOT NULL DEFAULT clock_timestamp(),
    completed_ts timestamptz,
    PRIMARY KEY (host_id, feature_id, logical_turn_id),
    UNIQUE(host_id,task_id),
    FOREIGN KEY(host_id,task_id) REFERENCES workflow_ops.task_info_t(host_id,task_id),
    FOREIGN KEY (host_id, claim_id) REFERENCES workflow_ops.development_stage_t(host_id, claim_id),
    CHECK ((result IS NULL) = (completed_ts IS NULL))
);

CREATE TABLE workflow_ops.development_execution_fence_t (
    host_id uuid NOT NULL,
    task_id uuid NOT NULL,
    claim_id text NOT NULL,
    execution_id uuid NOT NULL,
    fencing_token bigint NOT NULL CHECK(fencing_token>0),
    result_digest text NOT NULL,
    recorded_ts timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY(host_id,task_id),
    FOREIGN KEY(host_id,task_id) REFERENCES workflow_ops.task_info_t(host_id,task_id),
    FOREIGN KEY(host_id,claim_id) REFERENCES workflow_ops.development_stage_t(host_id,claim_id)
);

CREATE TABLE workflow_ops.development_transition_t (
    host_id uuid NOT NULL,
    feature_id text NOT NULL,
    operation_id uuid NOT NULL,
    request_digest text NOT NULL,
    result jsonb NOT NULL,
    created_ts timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY(host_id,feature_id,operation_id),
    FOREIGN KEY(host_id,feature_id) REFERENCES workflow_ops.development_feature_t(host_id,feature_id)
);

-- Backstop for event-driven/generic process insertion, checked at COMMIT so the
-- claim can be inserted after its process in the same acceptance transaction.
CREATE FUNCTION workflow_ops.development_process_claim_guard() RETURNS trigger
LANGUAGE plpgsql SET search_path = workflow_ops, pg_catalog AS $$
BEGIN
    IF NEW.definition_snapshot #> '{document,metadata,developmentWorkflowStage}' IS NOT NULL
       OR NEW.definition_snapshot #>> '{document,name}' IN
          ('feature-intake','feature-design','feature-plan','feature-implement','feature-finalize')
       OR EXISTS(SELECT 1 FROM wf_definition_t WHERE host_id=NEW.host_id AND wf_def_id=NEW.wf_def_id
                 AND name IN ('feature-intake','feature-design','feature-plan','feature-implement','feature-finalize')) THEN
        IF NOT EXISTS(SELECT 1 FROM development_stage_t s JOIN development_feature_t f
            ON f.host_id=s.host_id AND f.feature_id=s.feature_id
            WHERE s.host_id=NEW.host_id AND s.process_id=NEW.process_id
              AND s.workflow_instance_id::text=NEW.wf_instance_id
              AND f.record->'activeClaim'=s.receipt AND f.record->>'state'='active') THEN
            RAISE EXCEPTION 'development workflow process requires an active stage claim';
        END IF;
    END IF;
    RETURN NEW;
END $$;
CREATE CONSTRAINT TRIGGER development_process_claim_required
AFTER INSERT ON workflow_ops.process_info_t DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION workflow_ops.development_process_claim_guard();

CREATE FUNCTION workflow_ops.development_task_dispatch_guard() RETURNS trigger
LANGUAGE plpgsql SET search_path = workflow_ops, pg_catalog AS $$
DECLARE f development_feature_t%ROWTYPE; s development_stage_t%ROWTYPE;
BEGIN
    -- Guard new dispatch only; fencing must not block result/cleanup writes.
    IF (NEW.locked='Y' AND NEW.locked IS DISTINCT FROM OLD.locked)
       OR (NEW.scheduling_request_id IS NOT NULL AND NEW.scheduling_request_id IS DISTINCT FROM OLD.scheduling_request_id) THEN
        SELECT * INTO s FROM development_stage_t WHERE host_id=NEW.host_id AND process_id=NEW.process_id;
        IF FOUND THEN
            SELECT * INTO f FROM development_feature_t WHERE host_id=s.host_id AND feature_id=s.feature_id FOR SHARE;
            IF f.record->>'state' IS DISTINCT FROM 'active' OR f.record->'activeClaim' IS DISTINCT FROM s.receipt
               OR NOT EXISTS(SELECT 1 FROM development_vm_t v WHERE v.host_id=f.host_id
                   AND v.vm_id=f.vm_id AND v.generation=f.generation AND v.feature_id=f.feature_id) THEN
                RAISE EXCEPTION 'development stage dispatch is fenced';
            END IF;
        END IF;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER development_task_dispatch_required
BEFORE UPDATE OF locked,scheduling_request_id ON workflow_ops.task_info_t
FOR EACH ROW EXECUTE FUNCTION workflow_ops.development_task_dispatch_guard();

GRANT SELECT,INSERT,UPDATE,DELETE ON workflow_ops.development_vm_t,
    workflow_ops.development_feature_t,workflow_ops.development_stage_t,
    workflow_ops.development_turn_t,workflow_ops.development_transition_t,
    workflow_ops.development_execution_fence_t TO operations_workflow_runtime;
