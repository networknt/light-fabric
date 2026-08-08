-- Provider-insensitive embedding-space identity only. No vectors, input text,
-- provider/model identifiers, or endpoint data may enter the audit database.
ALTER TABLE llm_audit_event_t
    ADD COLUMN IF NOT EXISTS expected_embedding_space_id varchar(255),
    ADD COLUMN IF NOT EXISTS expected_embedding_space_revision bigint,
    ADD COLUMN IF NOT EXISTS selected_embedding_space_id varchar(255),
    ADD COLUMN IF NOT EXISTS selected_embedding_space_revision bigint;

ALTER TABLE llm_audit_event_t
    DROP CONSTRAINT IF EXISTS llm_audit_event_embedding_space_check,
    ADD CONSTRAINT llm_audit_event_embedding_space_check CHECK (
        (expected_embedding_space_id IS NULL) = (expected_embedding_space_revision IS NULL)
        AND (selected_embedding_space_id IS NULL) = (selected_embedding_space_revision IS NULL)
        AND (expected_embedding_space_revision IS NULL OR expected_embedding_space_revision > 0)
        AND (selected_embedding_space_revision IS NULL OR selected_embedding_space_revision > 0)
    );
