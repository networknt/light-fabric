ALTER TABLE llm_audit_event_t
    ADD COLUMN IF NOT EXISTS billing_subject_digest char(64);

COMMENT ON COLUMN llm_audit_event_t.billing_subject_digest IS
    'SHA-256 digest of the verified accounting subject; never used as the authorization principal.';
