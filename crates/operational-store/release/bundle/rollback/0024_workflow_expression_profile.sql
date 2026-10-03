-- Owner-run only, after deployment inventory verifies all writers honor the switch.
-- Phase 1 commits OFF independently: an active-run refusal must leave admission OFF.
BEGIN;
SET LOCAL search_path TO workflow_ops, pg_catalog;
DO $disable$
BEGIN
    PERFORM 1 FROM workflow_ops.workflow_expression_profile_policy_t
     WHERE profile_id='cel-workflow-v2' FOR UPDATE;
    IF NOT FOUND THEN RAISE EXCEPTION 'WORKFLOW_EXPRESSION_POLICY_MISSING'; END IF;
    UPDATE workflow_ops.workflow_expression_profile_policy_t
       SET admission_enabled=false, updated_ts=CURRENT_TIMESTAMP, updated_by=SESSION_USER
     WHERE profile_id='cel-workflow-v2';
END
$disable$;
COMMIT;
-- Phase 2 reacquires and holds the switch lock through checks and DDL.
BEGIN;
SET LOCAL search_path TO workflow_ops, pg_catalog;
DO $rollback$
DECLARE enabled boolean;
BEGIN
    SELECT admission_enabled INTO enabled FROM workflow_ops.workflow_expression_profile_policy_t
     WHERE profile_id='cel-workflow-v2' FOR UPDATE;
    IF NOT FOUND OR enabled IS DISTINCT FROM false THEN
        RAISE EXCEPTION 'WORKFLOW_EXPRESSION_ADMISSION_NOT_OFF';
    END IF;
    IF EXISTS (SELECT 1 FROM workflow_ops.process_info_t
               WHERE expression_profile <> 'cel-workflow-v1'
                 AND (status_code NOT IN ('C','F') OR completed_ts IS NULL)) THEN
        RAISE EXCEPTION 'WORKFLOW_EXPRESSION_ACTIVE_V2_PROCESS';
    END IF;
END
$rollback$;
DROP FUNCTION workflow_ops.workflow_claim_host_task_v2(uuid,integer,text[]);
-- Keep profile column, consistency CHECK, claim index, legacy-only claim v1,
-- disabled policy row and capability evidence. Applied migrations are unchanged.
COMMIT;
