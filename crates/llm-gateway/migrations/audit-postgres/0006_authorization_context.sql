-- Additive: old WAL events deserialize with no authorization context.
ALTER TABLE llm_audit_event_t ADD COLUMN IF NOT EXISTS authorization_context jsonb;
ALTER TABLE llm_audit_event_t DROP CONSTRAINT IF EXISTS llm_audit_authorization_context_check;
ALTER TABLE llm_audit_event_t ADD CONSTRAINT llm_audit_authorization_context_check
CHECK (authorization_context IS NULL OR (jsonb_typeof(authorization_context) = 'object'
 AND length(authorization_context::text) <= 8192
 AND NOT (authorization_context ?| ARRAY['token','authorization','scopeToken','claims','clientSecret'])));
