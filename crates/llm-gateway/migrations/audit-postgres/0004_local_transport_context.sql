-- Bounded transport diagnostics only. The gateway never writes URLs, DNS
-- answers, certificate bodies, prompts, vectors, or secret references here.
ALTER TABLE llm_audit_event_t
    ADD COLUMN IF NOT EXISTS transport_context jsonb;

ALTER TABLE llm_audit_event_t
    DROP CONSTRAINT IF EXISTS llm_audit_event_transport_context_check,
    ADD CONSTRAINT llm_audit_event_transport_context_check CHECK (
        transport_context IS NULL OR (
            jsonb_typeof(transport_context) = 'object'
            AND NOT (transport_context ?| ARRAY[
                'url','baseUrl','dnsAnswers','certificate','pem','prompt',
                'vector','secretReference','credentialRef','credentialReference'
            ])
            AND length(transport_context::text) <= 4096
        )
    );

COMMENT ON COLUMN llm_audit_event_t.transport_context IS
    'Bounded LLM endpoint/profile/runtime/capacity/pricing identifiers and digest prefixes; no content or secret references.';
