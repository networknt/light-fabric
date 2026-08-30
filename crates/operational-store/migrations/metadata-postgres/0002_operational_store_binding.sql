-- Phase 1: one immutable active Host/environment binding per operations DB.

CREATE TABLE IF NOT EXISTS operational_meta.operational_store_binding_t (
    binding_id UUID PRIMARY KEY,
    binding_version BIGINT NOT NULL,
    binding_digest VARCHAR(71) NOT NULL,
    scope_kind VARCHAR(32) NOT NULL,
    scope_id UUID NOT NULL,
    host_id UUID NOT NULL,
    environment VARCHAR(64) NOT NULL,
    database_identity VARCHAR(63) NOT NULL,
    deployment_profile VARCHAR(32) NOT NULL,
    schema_contract_generation BIGINT NOT NULL,
    created_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    activated_ts TIMESTAMPTZ,
    active BOOLEAN NOT NULL DEFAULT FALSE,
    CONSTRAINT operational_store_binding_version_ck CHECK (binding_version >= 1),
    CONSTRAINT operational_store_binding_digest_ck
        CHECK (binding_digest ~ '^sha256:[0-9a-f]{64}$'),
    CONSTRAINT operational_store_binding_scope_kind_ck
        CHECK (scope_kind = 'HOST_ENVIRONMENT'),
    CONSTRAINT operational_store_binding_environment_ck
        CHECK (environment ~ '^[a-z][a-z0-9_-]{0,63}$'),
    CONSTRAINT operational_store_binding_database_ck
        CHECK (database_identity = 'operations'),
    CONSTRAINT operational_store_binding_profile_ck
        CHECK (deployment_profile = 'DEV_DEDICATED'),
    CONSTRAINT operational_store_binding_generation_ck
        CHECK (schema_contract_generation >= 1),
    CONSTRAINT operational_store_binding_activation_ck
        CHECK (NOT active OR activated_ts IS NOT NULL)
);

CREATE UNIQUE INDEX IF NOT EXISTS operational_store_binding_one_active_idx
    ON operational_meta.operational_store_binding_t ((active))
    WHERE active;

CREATE OR REPLACE FUNCTION operational_meta.protect_active_operational_binding()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $function$
BEGIN
    IF OLD.active AND (
        NEW.binding_id IS DISTINCT FROM OLD.binding_id OR
        NEW.scope_kind IS DISTINCT FROM OLD.scope_kind OR
        NEW.scope_id IS DISTINCT FROM OLD.scope_id OR
        NEW.host_id IS DISTINCT FROM OLD.host_id OR
        NEW.environment IS DISTINCT FROM OLD.environment OR
        NEW.database_identity IS DISTINCT FROM OLD.database_identity OR
        NEW.deployment_profile IS DISTINCT FROM OLD.deployment_profile
    ) THEN
        RAISE EXCEPTION 'active operational store identity is immutable';
    END IF;
    RETURN NEW;
END
$function$;

DROP TRIGGER IF EXISTS protect_active_operational_binding_trg
    ON operational_meta.operational_store_binding_t;
CREATE TRIGGER protect_active_operational_binding_trg
BEFORE UPDATE ON operational_meta.operational_store_binding_t
FOR EACH ROW
EXECUTE FUNCTION operational_meta.protect_active_operational_binding();

ALTER TABLE operational_meta.operational_store_binding_t
    OWNER TO operations_meta_migrator;
ALTER FUNCTION operational_meta.protect_active_operational_binding()
    OWNER TO operations_meta_migrator;
REVOKE ALL ON operational_meta.operational_store_binding_t FROM PUBLIC;
REVOKE ALL ON FUNCTION operational_meta.protect_active_operational_binding() FROM PUBLIC;

