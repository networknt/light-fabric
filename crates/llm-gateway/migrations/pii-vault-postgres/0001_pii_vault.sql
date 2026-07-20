-- Dedicated regional PII vault. Never apply to Portal or the inference-audit DB.
CREATE TABLE IF NOT EXISTS llm_pii_vault_entry_t (
    host_digest char(64) NOT NULL,
    scope_digest char(64) NOT NULL,
    token_digest char(64) NOT NULL,
    encrypted_value bytea NOT NULL,
    key_reference varchar(512) NOT NULL CHECK (key_reference ~ '^[A-Za-z][A-Za-z0-9+.-]*://'),
    expires_ts timestamptz NOT NULL,
    created_ts timestamptz NOT NULL,
    PRIMARY KEY (host_digest, scope_digest, token_digest)
);

CREATE INDEX IF NOT EXISTS llm_pii_vault_expiry_idx
    ON llm_pii_vault_entry_t (expires_ts);

CREATE TABLE IF NOT EXISTS llm_pii_vault_access_t (
    access_id uuid PRIMARY KEY,
    access_ts timestamptz NOT NULL,
    gateway_instance varchar(255) NOT NULL,
    operation varchar(16) NOT NULL CHECK (operation IN ('insert','resolve','revoke','expire')),
    host_digest varchar(64) NOT NULL,
    scope_digest varchar(64) NOT NULL,
    token_digest varchar(64),
    outcome varchar(16) NOT NULL
);

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'llm_pii_vault_gateway') THEN
        CREATE ROLE llm_pii_vault_gateway NOLOGIN;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'llm_pii_vault_auditor') THEN
        CREATE ROLE llm_pii_vault_auditor NOLOGIN;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'llm_pii_vault_expirer') THEN
        CREATE ROLE llm_pii_vault_expirer NOLOGIN;
    END IF;
END
$$;

CREATE OR REPLACE FUNCTION llm_pii_vault_insert_exact(
    p_host_digest char(64), p_scope_digest char(64), p_token_digest char(64),
    p_encrypted_value bytea, p_key_reference varchar(512), p_expires_ts timestamptz
) RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
BEGIN
    IF p_expires_ts <= clock_timestamp() THEN
        RAISE EXCEPTION 'PII vault expiry must be in the future';
    END IF;
    INSERT INTO public.llm_pii_vault_entry_t
        (host_digest,scope_digest,token_digest,encrypted_value,key_reference,expires_ts,created_ts)
    VALUES
        (p_host_digest,p_scope_digest,p_token_digest,p_encrypted_value,p_key_reference,p_expires_ts,clock_timestamp())
    ON CONFLICT (host_digest,scope_digest,token_digest) DO NOTHING;
END
$$;

CREATE OR REPLACE FUNCTION llm_pii_vault_resolve_exact(
    p_host_digest char(64), p_scope_digest char(64), p_token_digest char(64)
) RETURNS TABLE (encrypted_value bytea, key_reference varchar(512))
LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
    SELECT entry.encrypted_value, entry.key_reference
      FROM public.llm_pii_vault_entry_t AS entry
     WHERE entry.host_digest = p_host_digest
       AND entry.scope_digest = p_scope_digest
       AND entry.token_digest = p_token_digest
       AND entry.expires_ts > clock_timestamp()
$$;

CREATE OR REPLACE FUNCTION llm_pii_vault_revoke_exact(
    p_host_digest char(64), p_scope_digest char(64), p_token_digest char(64)
) RETURNS bigint
LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
    WITH deleted AS (
        DELETE FROM public.llm_pii_vault_entry_t AS entry
         WHERE entry.host_digest = p_host_digest
           AND entry.scope_digest = p_scope_digest
           AND entry.token_digest = p_token_digest
        RETURNING 1
    ) SELECT count(*) FROM deleted
$$;

CREATE OR REPLACE FUNCTION llm_pii_vault_expire_before(p_deadline timestamptz)
RETURNS bigint
LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
    WITH deleted AS (
        DELETE FROM public.llm_pii_vault_entry_t WHERE expires_ts <= p_deadline RETURNING 1
    ) SELECT count(*) FROM deleted
$$;

CREATE OR REPLACE FUNCTION llm_pii_vault_record_access(
    p_access_id uuid, p_gateway_instance varchar(255), p_operation varchar(16),
    p_host_digest varchar(64), p_scope_digest varchar(64), p_token_digest varchar(64),
    p_outcome varchar(16)
) RETURNS void
LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
    INSERT INTO public.llm_pii_vault_access_t
        (access_id,access_ts,gateway_instance,operation,host_digest,scope_digest,token_digest,outcome)
    VALUES
        (p_access_id,clock_timestamp(),p_gateway_instance,p_operation,p_host_digest,p_scope_digest,p_token_digest,p_outcome)
$$;

REVOKE ALL ON llm_pii_vault_entry_t, llm_pii_vault_access_t FROM llm_pii_vault_gateway;
REVOKE ALL ON FUNCTION llm_pii_vault_insert_exact(char(64),char(64),char(64),bytea,varchar(512),timestamptz) FROM PUBLIC;
REVOKE ALL ON FUNCTION llm_pii_vault_resolve_exact(char(64),char(64),char(64)) FROM PUBLIC;
REVOKE ALL ON FUNCTION llm_pii_vault_revoke_exact(char(64),char(64),char(64)) FROM PUBLIC;
REVOKE ALL ON FUNCTION llm_pii_vault_expire_before(timestamptz) FROM PUBLIC;
REVOKE ALL ON FUNCTION llm_pii_vault_record_access(uuid,varchar(255),varchar(16),varchar(64),varchar(64),varchar(64),varchar(16)) FROM PUBLIC;

-- The gateway can invoke only exact-key functions and cannot SELECT the table.
GRANT EXECUTE ON FUNCTION llm_pii_vault_insert_exact(char(64),char(64),char(64),bytea,varchar(512),timestamptz) TO llm_pii_vault_gateway;
GRANT EXECUTE ON FUNCTION llm_pii_vault_resolve_exact(char(64),char(64),char(64)) TO llm_pii_vault_gateway;
GRANT EXECUTE ON FUNCTION llm_pii_vault_revoke_exact(char(64),char(64),char(64)) TO llm_pii_vault_gateway;
GRANT EXECUTE ON FUNCTION llm_pii_vault_record_access(uuid,varchar(255),varchar(16),varchar(64),varchar(64),varchar(64),varchar(16)) TO llm_pii_vault_gateway;
GRANT SELECT ON llm_pii_vault_access_t TO llm_pii_vault_auditor;
GRANT EXECUTE ON FUNCTION llm_pii_vault_expire_before(timestamptz) TO llm_pii_vault_expirer;
GRANT EXECUTE ON FUNCTION llm_pii_vault_record_access(uuid,varchar(255),varchar(16),varchar(64),varchar(64),varchar(64),varchar(16)) TO llm_pii_vault_expirer;

COMMENT ON TABLE llm_pii_vault_entry_t IS
    'Encrypted exact-token mappings with enforced expiry; no scan/export privilege is granted to the gateway.';
