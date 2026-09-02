-- P7: convert operational database metadata from the retired provider profile
-- to the Host-scoped customer-managed registration contract.

DROP TRIGGER IF EXISTS protect_active_operational_binding_trg
    ON operational_meta.operational_store_binding_t;

ALTER TABLE operational_meta.operational_store_binding_t
    DROP CONSTRAINT IF EXISTS operational_store_binding_scope_kind_ck,
    DROP CONSTRAINT IF EXISTS operational_store_binding_environment_ck,
    DROP CONSTRAINT IF EXISTS operational_store_binding_profile_ck,
    ALTER COLUMN environment DROP NOT NULL;

UPDATE operational_meta.operational_store_binding_t
SET binding_version = GREATEST(binding_version, 2),
    scope_kind = 'HOST',
    scope_id = host_id,
    environment = NULL,
    deployment_profile = 'CUSTOMER_MANAGED';

ALTER TABLE operational_meta.operational_store_binding_t
    ADD CONSTRAINT operational_store_binding_scope_kind_ck
        CHECK (scope_kind = 'HOST'),
    ADD CONSTRAINT operational_store_binding_environment_ck
        CHECK (environment IS NULL),
    ADD CONSTRAINT operational_store_binding_profile_ck
        CHECK (deployment_profile = 'CUSTOMER_MANAGED'),
    ADD CONSTRAINT operational_store_binding_version_v2_ck
        CHECK (binding_version >= 2);

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
        NEW.database_identity IS DISTINCT FROM OLD.database_identity OR
        NEW.deployment_profile IS DISTINCT FROM OLD.deployment_profile
    ) THEN
        RAISE EXCEPTION 'active operational store identity is immutable';
    END IF;
    RETURN NEW;
END
$function$;

ALTER FUNCTION operational_meta.protect_active_operational_binding()
    OWNER TO operations_meta_migrator;

CREATE TRIGGER protect_active_operational_binding_trg
BEFORE UPDATE ON operational_meta.operational_store_binding_t
FOR EACH ROW
EXECUTE FUNCTION operational_meta.protect_active_operational_binding();
