-- P01: forward-only timer support; historical migrations remain unchanged.
SET search_path TO workflow_ops, pg_catalog;
CREATE TABLE workflow_ops.workflow_task_timer_t (
    host_id uuid NOT NULL,
    task_id uuid NOT NULL,
    process_id uuid NOT NULL,
    duration_seconds integer NOT NULL CHECK(duration_seconds BETWEEN 1 AND 600),
    armed_at timestamptz NOT NULL,
    wake_at timestamptz NOT NULL,
    -- NULL preserves private v1 execution with no explicit workflow deadline.
    effective_deadline timestamptz,
    state text NOT NULL CHECK(state IN('ARMED','FIRED','CANCELLED','EXPIRED','FAILED')),
    generation bigint NOT NULL DEFAULT 1 CHECK(generation>0),
    task_fence bigint NOT NULL CHECK(task_fence>0),
    successor_task_id uuid,
    wake_failure_count smallint NOT NULL DEFAULT 0 CHECK(wake_failure_count BETWEEN 0 AND 3),
    retry_after_ts timestamptz NOT NULL DEFAULT clock_timestamp(),
    last_failure_code varchar(64),
    updated_ts timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY(host_id,task_id),
    FOREIGN KEY(host_id,task_id) REFERENCES workflow_ops.task_info_t(host_id,task_id) ON DELETE CASCADE,
    FOREIGN KEY(host_id,process_id) REFERENCES workflow_ops.process_info_t(host_id,process_id) ON DELETE CASCADE,
    FOREIGN KEY(host_id,successor_task_id) REFERENCES workflow_ops.task_info_t(host_id,task_id),
    CHECK(successor_task_id IS NULL OR state='FIRED')
);
CREATE INDEX workflow_task_timer_due_idx ON workflow_ops.workflow_task_timer_t(wake_at,host_id,task_id) WHERE state='ARMED';

-- Cancellation/deadline paths already terminalize task rows under parent locks.
-- Keep timer state in that same transaction without new public cancel APIs.
CREATE FUNCTION workflow_ops.workflow_timer_terminal_task_v1() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.status_code='F' THEN
        UPDATE workflow_ops.workflow_task_timer_t
           SET state=CASE WHEN NEW.result_code LIKE '%TIMEOUT%' THEN 'EXPIRED' WHEN NEW.result_code='WORKFLOW_TIMER_FAILED' THEN 'FAILED' ELSE 'CANCELLED' END,
               updated_ts=clock_timestamp()
         WHERE host_id=NEW.host_id AND task_id=NEW.task_id AND state='ARMED';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER workflow_timer_terminal_task_v1 AFTER UPDATE OF status_code ON workflow_ops.task_info_t
FOR EACH ROW WHEN(NEW.task_type='wait') EXECUTE FUNCTION workflow_ops.workflow_timer_terminal_task_v1();

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
          FROM task_info_t t
         WHERE t.active AND t.execution_placement='host'
           AND ((t.status_code='A' AND t.task_type IN ('ask','assert','call','set','switch','fork','wait'))
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
         AND ((t.status_code='A' AND t.task_type IN ('ask','assert','call','set','switch','fork','wait'))
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

GRANT SELECT,INSERT,UPDATE ON workflow_ops.workflow_task_timer_t TO operations_workflow_runtime;
GRANT EXECUTE ON FUNCTION workflow_ops.workflow_claim_host_task_v1(uuid,integer) TO operations_workflow_runtime;
