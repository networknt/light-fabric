-- Agent policy digests include the sha256: prefix (71 characters). Preserve
-- legacy 64-character workflow digests and widen every remaining execution
-- policy field that receives the same authenticated value.
ALTER TABLE execution_ops.execution_session_t
    ALTER COLUMN policy_digest TYPE VARCHAR(71),
    ALTER COLUMN hold_policy_digest TYPE VARCHAR(71);
ALTER TABLE execution_ops.execution_runtime_audit_t
    ALTER COLUMN policy_digest TYPE VARCHAR(71);
