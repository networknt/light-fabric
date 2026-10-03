-- E04 W2 forward-only schema protection; admission remains OFF.
BEGIN;
SET LOCAL search_path TO workflow_ops, pg_catalog;
-- Text definitions may be YAML. Conservatively reject literal reserved-key
-- occurrences and backslash-encoded key ambiguity; never rewrite definitions.
-- Escapes (including YAML line continuation) require owner inspection first.
LOCK TABLE workflow_ops.wf_definition_t, workflow_ops.wf_definition_version_t,
    workflow_ops.process_info_t IN SHARE MODE;
DO $preflight$
BEGIN
    IF EXISTS (SELECT 1 FROM workflow_ops.wf_definition_t
               WHERE position('lightExpressionProfile' in definition)>0)
       OR EXISTS (SELECT 1 FROM workflow_ops.wf_definition_version_t
                  WHERE position('lightExpressionProfile' in definition)>0)
       OR EXISTS (SELECT 1 FROM workflow_ops.process_info_t
                  WHERE COALESCE((definition_snapshot #> '{document,metadata}')
                                 ? 'lightExpressionProfile', false)) THEN
        RAISE EXCEPTION 'WORKFLOW_EXPRESSION_RESERVED_KEY_COLLISION';
    END IF;
    IF EXISTS (SELECT 1 FROM workflow_ops.wf_definition_t
               WHERE position(chr(92) in definition)>0)
       OR EXISTS (SELECT 1 FROM workflow_ops.wf_definition_version_t
                  WHERE position(chr(92) in definition)>0) THEN
        RAISE EXCEPTION 'WORKFLOW_EXPRESSION_RESERVED_KEY_AMBIGUITY'
            USING HINT = 'Stop deployment. Inspect escaped YAML/JSON definitions and immutable versions with an owner-approved parser; do not remove backslashes, rewrite data, or bypass preflight automatically.';
    END IF;
END
$preflight$;
ALTER TABLE workflow_ops.process_info_t
    ADD COLUMN expression_profile varchar(64) NOT NULL DEFAULT 'cel-workflow-v1',
    ADD CONSTRAINT process_expression_profile_snapshot_ck CHECK (
        (CASE WHEN NOT COALESCE((definition_snapshot #> '{document,metadata}')
                               ? 'lightExpressionProfile', false)
          THEN expression_profile = 'cel-workflow-v1'
          ELSE expression_profile = 'cel-workflow-v2'
               AND definition_snapshot IS NOT NULL
               AND jsonb_typeof(definition_snapshot #> '{document,metadata,lightExpressionProfile}') = 'string'
               AND definition_snapshot #> '{document,metadata,lightExpressionProfile}' = '"cel-workflow-v2"'::jsonb
         END) IS TRUE
    );
CREATE INDEX process_expression_profile_claim_idx
    ON workflow_ops.process_info_t(expression_profile, host_id, process_id);
CREATE TABLE workflow_ops.workflow_expression_profile_policy_t (
    profile_id varchar(64) PRIMARY KEY,
    admission_enabled boolean NOT NULL DEFAULT false,
    updated_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_by varchar(126) NOT NULL DEFAULT SESSION_USER
);
INSERT INTO workflow_ops.workflow_expression_profile_policy_t(profile_id, admission_enabled)
VALUES ('cel-workflow-v2', false);
CREATE TABLE workflow_ops.workflow_worker_capability_t (
    instance_id uuid PRIMARY KEY,
    binary_version text NOT NULL,
    supported_profiles text[] NOT NULL,
    admits_profiles text[] NOT NULL,
    heartbeat_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP
);
COMMENT ON COLUMN workflow_ops.process_info_t.expression_profile IS 'Immutable expression evaluator profile selected from the process definition snapshot.';
COMMENT ON TABLE workflow_ops.workflow_expression_profile_policy_t IS 'Deployment-controlled admission switch; application admission must lock FOR SHARE in its admitting transaction.';
COMMENT ON TABLE workflow_ops.workflow_worker_capability_t IS 'Worker capability heartbeat evidence; absence does not prove that no old worker exists.';
CREATE OR REPLACE FUNCTION workflow_ops.workflow_claim_host_task_v1(p_worker_id uuid, p_lease_ms integer) RETURNS TABLE(host_id uuid, task_id uuid, task_type character varying, process_id uuid, wf_instance_id character varying, wf_task_id character varying, status_code character, result_code character varying, lease_owner uuid, lease_fencing_token bigint, lease_expires_ts timestamp with time zone)
    LANGUAGE plpgsql
    AS $$
DECLARE claimed_host UUID;
BEGIN
    IF p_lease_ms<100 OR p_lease_ms>30000 THEN RAISE EXCEPTION 'WORKFLOW_HOST_LEASE_MS_OUT_OF_RANGE'; END IF;
    SELECT candidates.host_id INTO claimed_host FROM (
        SELECT t.host_id,
               MIN(CASE t.execution_class WHEN 'interactive' THEN 0 WHEN 'standard' THEN 1 ELSE 2 END) class_rank,
               MAX(t.priority) maximum_priority,MIN(t.started_ts) oldest_task
          FROM workflow_ops.task_info_t t JOIN workflow_ops.process_info_t p ON p.host_id=t.host_id AND p.process_id=t.process_id
         WHERE p.expression_profile='cel-workflow-v1' AND t.active AND t.execution_placement='host'
           AND ((t.status_code='A' AND t.task_type IN ('ask','assert','call','set','switch','fork','wait'))
             OR (t.status_code='C' AND t.task_type='ask' AND t.completed_ts IS NOT NULL
                 AND (t.task_output IS NULL OR t.task_output->>'status'='waiting_for_input')))
           AND t.next_attempt_ts<=CURRENT_TIMESTAMP
           AND (t.effect_state='none' OR t.downstream_idempotency_key IS NOT NULL)
           AND (t.locked='N' OR (t.locked='Y' AND t.lease_expires_ts<=CURRENT_TIMESTAMP))
           AND (t.deadline_ts IS NULL OR t.deadline_ts>CURRENT_TIMESTAMP)
         GROUP BY t.host_id
    ) candidates LEFT JOIN workflow_ops.workflow_executor_tenant_turn_t turn ON turn.host_id=candidates.host_id
    ORDER BY candidates.class_rank,COALESCE(turn.last_claim_ts,'-infinity'::timestamptz),
             candidates.maximum_priority DESC,candidates.oldest_task,candidates.host_id LIMIT 1;
    IF claimed_host IS NULL THEN RETURN; END IF;
    IF NOT pg_try_advisory_xact_lock(hashtext(claimed_host::text)) THEN RETURN; END IF;
    INSERT INTO workflow_ops.workflow_executor_tenant_turn_t(host_id,last_claim_ts,claim_count)
    VALUES(claimed_host,CURRENT_TIMESTAMP,1)
    ON CONFLICT ON CONSTRAINT workflow_executor_tenant_turn_t_pkey DO UPDATE SET
      last_claim_ts=EXCLUDED.last_claim_ts,claim_count=workflow_executor_tenant_turn_t.claim_count+1,
      updated_ts=CURRENT_TIMESTAMP;
    RETURN QUERY WITH candidate AS (
      SELECT t.host_id,t.task_id FROM workflow_ops.task_info_t t JOIN workflow_ops.process_info_t p ON p.host_id=t.host_id AND p.process_id=t.process_id
       WHERE p.expression_profile='cel-workflow-v1' AND t.host_id=claimed_host AND t.active AND t.execution_placement='host'
         AND ((t.status_code='A' AND t.task_type IN ('ask','assert','call','set','switch','fork','wait'))
           OR (t.status_code='C' AND t.task_type='ask' AND t.completed_ts IS NOT NULL
               AND (t.task_output IS NULL OR t.task_output->>'status'='waiting_for_input')))
         AND t.next_attempt_ts<=CURRENT_TIMESTAMP
         AND (t.effect_state='none' OR t.downstream_idempotency_key IS NOT NULL)
         AND (t.locked='N' OR (t.locked='Y' AND t.lease_expires_ts<=CURRENT_TIMESTAMP))
         AND (t.deadline_ts IS NULL OR t.deadline_ts>CURRENT_TIMESTAMP)
       ORDER BY CASE t.execution_class WHEN 'interactive' THEN 0 WHEN 'standard' THEN 1 ELSE 2 END,
                t.priority DESC,t.started_ts,t.task_id LIMIT 1 FOR UPDATE OF t SKIP LOCKED
    ) UPDATE workflow_ops.task_info_t t SET locked='Y',lease_owner=p_worker_id,
      lease_fencing_token=t.lease_fencing_token+1,
      lease_expires_ts=LEAST(COALESCE(t.deadline_ts,'infinity'::timestamptz),
        CURRENT_TIMESTAMP+make_interval(secs=>p_lease_ms::double precision/1000.0)),update_ts=CURRENT_TIMESTAMP
      FROM candidate c WHERE t.host_id=c.host_id AND t.task_id=c.task_id
    RETURNING t.host_id,t.task_id,t.task_type,t.process_id,t.wf_instance_id,t.wf_task_id,
              t.status_code,t.result_code,t.lease_owner,t.lease_fencing_token,t.lease_expires_ts;
END
$$;
CREATE OR REPLACE FUNCTION workflow_ops.workflow_claim_host_task_v2(p_worker_id uuid, p_lease_ms integer, p_supported_profiles text[]) RETURNS TABLE(host_id uuid, task_id uuid, task_type character varying, process_id uuid, wf_instance_id character varying, wf_task_id character varying, status_code character, result_code character varying, lease_owner uuid, lease_fencing_token bigint, lease_expires_ts timestamp with time zone)
    LANGUAGE plpgsql
    AS $$
DECLARE claimed_host UUID;
BEGIN
    IF p_lease_ms<100 OR p_lease_ms>30000 THEN RAISE EXCEPTION 'WORKFLOW_HOST_LEASE_MS_OUT_OF_RANGE'; END IF;
    SELECT candidates.host_id INTO claimed_host FROM (
        SELECT t.host_id,
               MIN(CASE t.execution_class WHEN 'interactive' THEN 0 WHEN 'standard' THEN 1 ELSE 2 END) class_rank,
               MAX(t.priority) maximum_priority,MIN(t.started_ts) oldest_task
          FROM workflow_ops.task_info_t t JOIN workflow_ops.process_info_t p ON p.host_id=t.host_id AND p.process_id=t.process_id
         WHERE p.expression_profile=ANY(p_supported_profiles) AND t.active AND t.execution_placement='host'
           AND ((t.status_code='A' AND t.task_type IN ('ask','assert','call','set','switch','fork','wait'))
             OR (t.status_code='C' AND t.task_type='ask' AND t.completed_ts IS NOT NULL
                 AND (t.task_output IS NULL OR t.task_output->>'status'='waiting_for_input')))
           AND t.next_attempt_ts<=CURRENT_TIMESTAMP
           AND (t.effect_state='none' OR t.downstream_idempotency_key IS NOT NULL)
           AND (t.locked='N' OR (t.locked='Y' AND t.lease_expires_ts<=CURRENT_TIMESTAMP))
           AND (t.deadline_ts IS NULL OR t.deadline_ts>CURRENT_TIMESTAMP)
         GROUP BY t.host_id
    ) candidates LEFT JOIN workflow_ops.workflow_executor_tenant_turn_t turn ON turn.host_id=candidates.host_id
    ORDER BY candidates.class_rank,COALESCE(turn.last_claim_ts,'-infinity'::timestamptz),
             candidates.maximum_priority DESC,candidates.oldest_task,candidates.host_id LIMIT 1;
    IF claimed_host IS NULL THEN RETURN; END IF;
    IF NOT pg_try_advisory_xact_lock(hashtext(claimed_host::text)) THEN RETURN; END IF;
    INSERT INTO workflow_ops.workflow_executor_tenant_turn_t(host_id,last_claim_ts,claim_count)
    VALUES(claimed_host,CURRENT_TIMESTAMP,1)
    ON CONFLICT ON CONSTRAINT workflow_executor_tenant_turn_t_pkey DO UPDATE SET
      last_claim_ts=EXCLUDED.last_claim_ts,claim_count=workflow_executor_tenant_turn_t.claim_count+1,
      updated_ts=CURRENT_TIMESTAMP;
    RETURN QUERY WITH candidate AS (
      SELECT t.host_id,t.task_id FROM workflow_ops.task_info_t t JOIN workflow_ops.process_info_t p ON p.host_id=t.host_id AND p.process_id=t.process_id
       WHERE p.expression_profile=ANY(p_supported_profiles) AND t.host_id=claimed_host AND t.active AND t.execution_placement='host'
         AND ((t.status_code='A' AND t.task_type IN ('ask','assert','call','set','switch','fork','wait'))
           OR (t.status_code='C' AND t.task_type='ask' AND t.completed_ts IS NOT NULL
               AND (t.task_output IS NULL OR t.task_output->>'status'='waiting_for_input')))
         AND t.next_attempt_ts<=CURRENT_TIMESTAMP
         AND (t.effect_state='none' OR t.downstream_idempotency_key IS NOT NULL)
         AND (t.locked='N' OR (t.locked='Y' AND t.lease_expires_ts<=CURRENT_TIMESTAMP))
         AND (t.deadline_ts IS NULL OR t.deadline_ts>CURRENT_TIMESTAMP)
       ORDER BY CASE t.execution_class WHEN 'interactive' THEN 0 WHEN 'standard' THEN 1 ELSE 2 END,
                t.priority DESC,t.started_ts,t.task_id LIMIT 1 FOR UPDATE OF t SKIP LOCKED
    ) UPDATE workflow_ops.task_info_t t SET locked='Y',lease_owner=p_worker_id,
      lease_fencing_token=t.lease_fencing_token+1,
      lease_expires_ts=LEAST(COALESCE(t.deadline_ts,'infinity'::timestamptz),
        CURRENT_TIMESTAMP+make_interval(secs=>p_lease_ms::double precision/1000.0)),update_ts=CURRENT_TIMESTAMP
      FROM candidate c WHERE t.host_id=c.host_id AND t.task_id=c.task_id
    RETURNING t.host_id,t.task_id,t.task_type,t.process_id,t.wf_instance_id,t.wf_task_id,
              t.status_code,t.result_code,t.lease_owner,t.lease_fencing_token,t.lease_expires_ts;
END
$$;
-- Policy reads need UPDATE privilege for PostgreSQL row locking, but only
-- the key column is granted: runtime cannot toggle admission_enabled.
REVOKE ALL ON workflow_ops.workflow_expression_profile_policy_t FROM PUBLIC;
REVOKE ALL ON workflow_ops.workflow_worker_capability_t FROM PUBLIC;
-- 0001 gives this role full DML on migrator-created future tables.
-- GRANT is additive: remove those inherited creation-time ACLs first.
REVOKE ALL ON workflow_ops.workflow_expression_profile_policy_t FROM operations_workflow_runtime;
REVOKE ALL ON workflow_ops.workflow_worker_capability_t FROM operations_workflow_runtime;
GRANT SELECT, UPDATE(profile_id) ON workflow_ops.workflow_expression_profile_policy_t TO operations_workflow_runtime;
GRANT SELECT, INSERT, UPDATE ON workflow_ops.workflow_worker_capability_t TO operations_workflow_runtime;
REVOKE ALL ON FUNCTION workflow_ops.workflow_claim_host_task_v2(uuid,integer,text[]) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION workflow_ops.workflow_claim_host_task_v2(uuid,integer,text[]) TO operations_workflow_runtime;
GRANT EXECUTE ON FUNCTION workflow_ops.workflow_claim_host_task_v1(uuid,integer) TO operations_workflow_runtime;
COMMIT;
