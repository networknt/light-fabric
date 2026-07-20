-- PII audit remains metadata-only: record only the versioned profile identity.
DO $$
DECLARE
    constraint_name text;
BEGIN
    SELECT conname INTO constraint_name
      FROM pg_constraint
     WHERE conrelid = 'llm_audit_event_t'::regclass
       AND contype = 'c'
       AND pg_get_constraintdef(oid) LIKE '%pii_profile%';
    IF constraint_name IS NOT NULL THEN
        EXECUTE format('ALTER TABLE llm_audit_event_t DROP CONSTRAINT %I', constraint_name);
    END IF;
END
$$;

ALTER TABLE llm_audit_event_t
    ADD CONSTRAINT llm_audit_event_pii_profile_check
    CHECK (
        pii_profile = 'none'
        OR pii_profile ~ '^[A-Za-z0-9._-]+:v[0-9]+:(request|session|host)$'
    );
