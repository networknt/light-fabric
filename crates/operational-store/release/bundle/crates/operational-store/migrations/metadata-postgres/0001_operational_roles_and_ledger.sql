-- Phase 1: cluster roles, operational_meta, and the immutable migration ledger.
-- Database creation remains a deployment-bootstrap responsibility.

DO $roles$
DECLARE
    role_name TEXT;
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'operations_bootstrap_admin') THEN
        CREATE ROLE operations_bootstrap_admin NOLOGIN CREATEDB CREATEROLE;
    END IF;

    FOREACH role_name IN ARRAY ARRAY[
        'operations_meta_migrator',
        'operations_execution_migrator',
        'operations_agent_migrator',
        'operations_a2a_migrator',
        'operations_workflow_migrator',
        'operations_gateway_migrator',
        'operations_audit_migrator',
        'operations_artifact_migrator',
        'operations_execution_runtime',
        'operations_agent_runtime',
        'operations_a2a_runtime',
        'operations_workflow_runtime',
        'operations_gateway_runtime',
        'operations_audit_publisher',
        'operations_artifact_runtime'
    ]
    LOOP
        IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = role_name) THEN
            EXECUTE format('CREATE ROLE %I NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT', role_name);
        END IF;
    END LOOP;
END
$roles$;

ALTER ROLE operations_bootstrap_admin NOLOGIN CREATEDB CREATEROLE NOSUPERUSER NOREPLICATION;

DO $harden_roles$
DECLARE
    role_name TEXT;
BEGIN
    FOREACH role_name IN ARRAY ARRAY[
        'operations_meta_migrator',
        'operations_execution_migrator',
        'operations_agent_migrator',
        'operations_a2a_migrator',
        'operations_workflow_migrator',
        'operations_gateway_migrator',
        'operations_audit_migrator',
        'operations_artifact_migrator',
        'operations_execution_runtime',
        'operations_agent_runtime',
        'operations_a2a_runtime',
        'operations_workflow_runtime',
        'operations_gateway_runtime',
        'operations_audit_publisher',
        'operations_artifact_runtime'
    ]
    LOOP
        EXECUTE format(
            'ALTER ROLE %I NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS',
            role_name
        );
    END LOOP;
END
$harden_roles$;

REVOKE CREATE, TEMPORARY ON DATABASE operations FROM PUBLIC;
REVOKE CREATE ON SCHEMA public FROM PUBLIC;

CREATE SCHEMA IF NOT EXISTS operational_meta AUTHORIZATION operations_meta_migrator;
ALTER SCHEMA operational_meta OWNER TO operations_meta_migrator;
REVOKE ALL ON SCHEMA operational_meta FROM PUBLIC;

CREATE TABLE IF NOT EXISTS operational_meta.operational_schema_migration_t (
    migration_owner VARCHAR(64) NOT NULL,
    schema_name VARCHAR(63) NOT NULL,
    migration_id VARCHAR(128) NOT NULL,
    migration_digest VARCHAR(71) NOT NULL,
    bundle_version VARCHAR(32) NOT NULL,
    contract_generation BIGINT NOT NULL,
    applied_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT operational_schema_migration_pk
        PRIMARY KEY (migration_owner, schema_name, migration_id),
    CONSTRAINT operational_schema_migration_digest_ck
        CHECK (migration_digest ~ '^sha256:[0-9a-f]{64}$'),
    CONSTRAINT operational_schema_migration_generation_ck
        CHECK (contract_generation >= 1)
);

ALTER TABLE operational_meta.operational_schema_migration_t
    OWNER TO operations_meta_migrator;
REVOKE ALL ON operational_meta.operational_schema_migration_t FROM PUBLIC;

