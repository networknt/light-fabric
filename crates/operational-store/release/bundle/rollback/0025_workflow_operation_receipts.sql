-- Disable fresh v2 admission. Immutable receipts remain queryable indefinitely.
BEGIN;
UPDATE workflow_ops.workflow_expression_profile_policy_t
SET admission_enabled=false WHERE profile_id='cel-workflow-v2';
COMMIT;
