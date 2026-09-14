-- Read-only. Use the issuer schema search_path after installing the A1 patch.
-- Lists names and declared sources only; never prints values or credentials.
SELECT c.host_id, c.client_id, c.client_name, names.claim_name,
       s.source_kind, s.attribute_id,
       CASE WHEN s.claim_name IS NULL THEN 'CLASSIFICATION_REQUIRED' ELSE 'DECLARED' END AS status
FROM auth_client_t c
CROSS JOIN LATERAL jsonb_object_keys(COALESCE(NULLIF(c.custom_claim,''),'{}')::jsonb) AS names(claim_name)
LEFT JOIN auth_refresh_claim_source_t s
  ON s.auth_host_id=c.host_id AND s.client_id=c.client_id AND s.claim_name=names.claim_name
WHERE c.active AND names.claim_name NOT IN
 ('token_use','iss','aud','exp','iat','nbf','jti','kid','client_id','scope','cid','scp','sub',
  'uid','uty','role','grp','pos','att','host','hostId','host_id','eid','eml','csrf')
ORDER BY c.host_id,c.client_name,names.claim_name;
