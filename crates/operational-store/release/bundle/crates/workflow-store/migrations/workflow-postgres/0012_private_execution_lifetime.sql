SET search_path TO workflow_ops, pg_catalog;

-- Private root human waits outlive the bounded admission/action credential.
-- A NULL budget deadline is allowed only with the versioned private lifetime
-- snapshot; ordinary and child invocations retain their declared deadline.
ALTER TABLE workflow_ops.workflow_invocation_budget_t
    ALTER COLUMN deadline_ts DROP NOT NULL;

ALTER TABLE workflow_ops.workflow_invocation_budget_t
    ADD COLUMN lifetime_version smallint;

ALTER TABLE workflow_ops.workflow_invocation_budget_t
    ADD CONSTRAINT workflow_invocation_budget_private_lifetime_ck CHECK (
        (deadline_ts IS NOT NULL AND lifetime_version IS NULL)
        OR (deadline_ts IS NULL AND lifetime_version=1)
    );

CREATE OR REPLACE FUNCTION workflow_ops.workflow_reserve_budget_v1(
    p_host_id uuid, p_ledger_id uuid, p_reservation_id uuid,
    p_generation bigint, p_fencing_token bigint, p_task_attempts bigint,
    p_nested_calls bigint, p_bytes bigint, p_cost_units bigint
) RETURNS boolean LANGUAGE plpgsql AS $$
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
       AND generation=p_generation
       AND (deadline_ts>CURRENT_TIMESTAMP OR (deadline_ts IS NULL AND lifetime_version=1))
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
