-- Sanitized canary evidence. No prompt, completion, principal, credential, or
-- provider error content is selected.
SELECT
    public_alias,
    snapshot_digest AS publication_digest,
    generation::text AS policy_version,
    snapshot_digest AS pricing_version,
    status AS attempt_outcome,
    category AS attempt_category,
    count(*) AS attempt_count
FROM llm_audit_event_t
WHERE event_kind = 'attempt_finished'
  AND event_ts >= now() - interval '30 minutes'
GROUP BY 1, 2, 3, 4, 5, 6
ORDER BY 1, 2, 3, 4, 5, 6;
