-- Compatibility rollback is non-destructive: admission OFF, retain authority and receipts.
-- Do not drop tables/triggers or reconstruct these records from events.
BEGIN;
UPDATE public.workflow_expression_profile_policy_t
   SET admission_enabled = false, updated_ts = CURRENT_TIMESTAMP, updated_by = SESSION_USER
 WHERE profile_id = 'cel-workflow-v2';
COMMIT;
