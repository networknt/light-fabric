-- Phase 1: establish ownership boundaries without moving application tables.

CREATE SCHEMA IF NOT EXISTS execution_ops AUTHORIZATION operations_execution_migrator;
CREATE SCHEMA IF NOT EXISTS agent_ops AUTHORIZATION operations_agent_migrator;
CREATE SCHEMA IF NOT EXISTS a2a_ops AUTHORIZATION operations_a2a_migrator;
CREATE SCHEMA IF NOT EXISTS workflow_ops AUTHORIZATION operations_workflow_migrator;
CREATE SCHEMA IF NOT EXISTS gateway_ops AUTHORIZATION operations_gateway_migrator;
CREATE SCHEMA IF NOT EXISTS audit_ops AUTHORIZATION operations_audit_migrator;
CREATE SCHEMA IF NOT EXISTS artifact_ops AUTHORIZATION operations_artifact_migrator;

ALTER SCHEMA execution_ops OWNER TO operations_execution_migrator;
ALTER SCHEMA agent_ops OWNER TO operations_agent_migrator;
ALTER SCHEMA a2a_ops OWNER TO operations_a2a_migrator;
ALTER SCHEMA workflow_ops OWNER TO operations_workflow_migrator;
ALTER SCHEMA gateway_ops OWNER TO operations_gateway_migrator;
ALTER SCHEMA audit_ops OWNER TO operations_audit_migrator;
ALTER SCHEMA artifact_ops OWNER TO operations_artifact_migrator;

REVOKE ALL ON SCHEMA execution_ops, agent_ops, a2a_ops, workflow_ops,
    gateway_ops, audit_ops, artifact_ops FROM PUBLIC;

GRANT CONNECT ON DATABASE operations TO
    operations_meta_migrator,
    operations_execution_migrator,
    operations_agent_migrator,
    operations_a2a_migrator,
    operations_workflow_migrator,
    operations_gateway_migrator,
    operations_audit_migrator,
    operations_artifact_migrator,
    operations_execution_runtime,
    operations_agent_runtime,
    operations_a2a_runtime,
    operations_workflow_runtime,
    operations_gateway_runtime,
    operations_audit_publisher,
    operations_artifact_runtime;

GRANT USAGE ON SCHEMA operational_meta TO
    operations_execution_runtime,
    operations_agent_runtime,
    operations_a2a_runtime,
    operations_workflow_runtime,
    operations_gateway_runtime,
    operations_audit_publisher,
    operations_artifact_runtime;
GRANT SELECT ON operational_meta.operational_store_binding_t TO
    operations_execution_runtime,
    operations_agent_runtime,
    operations_a2a_runtime,
    operations_workflow_runtime,
    operations_gateway_runtime,
    operations_audit_publisher,
    operations_artifact_runtime;

GRANT USAGE ON SCHEMA execution_ops TO operations_execution_runtime;
GRANT USAGE ON SCHEMA agent_ops TO operations_agent_runtime;
GRANT USAGE ON SCHEMA a2a_ops TO operations_a2a_runtime;
GRANT USAGE ON SCHEMA workflow_ops TO operations_workflow_runtime;
GRANT USAGE ON SCHEMA gateway_ops TO operations_gateway_runtime;
GRANT USAGE ON SCHEMA audit_ops TO operations_audit_publisher;
GRANT USAGE ON SCHEMA artifact_ops TO operations_artifact_runtime;

ALTER DEFAULT PRIVILEGES FOR ROLE operations_execution_migrator IN SCHEMA execution_ops
    GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO operations_execution_runtime;
ALTER DEFAULT PRIVILEGES FOR ROLE operations_agent_migrator IN SCHEMA agent_ops
    GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO operations_agent_runtime;
ALTER DEFAULT PRIVILEGES FOR ROLE operations_a2a_migrator IN SCHEMA a2a_ops
    GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO operations_a2a_runtime;
ALTER DEFAULT PRIVILEGES FOR ROLE operations_workflow_migrator IN SCHEMA workflow_ops
    GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO operations_workflow_runtime;
ALTER DEFAULT PRIVILEGES FOR ROLE operations_gateway_migrator IN SCHEMA gateway_ops
    GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO operations_gateway_runtime;
ALTER DEFAULT PRIVILEGES FOR ROLE operations_audit_migrator IN SCHEMA audit_ops
    GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO operations_audit_publisher;
ALTER DEFAULT PRIVILEGES FOR ROLE operations_artifact_migrator IN SCHEMA artifact_ops
    GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO operations_artifact_runtime;

ALTER DEFAULT PRIVILEGES FOR ROLE operations_execution_migrator IN SCHEMA execution_ops
    GRANT USAGE, SELECT, UPDATE ON SEQUENCES TO operations_execution_runtime;
ALTER DEFAULT PRIVILEGES FOR ROLE operations_agent_migrator IN SCHEMA agent_ops
    GRANT USAGE, SELECT, UPDATE ON SEQUENCES TO operations_agent_runtime;
ALTER DEFAULT PRIVILEGES FOR ROLE operations_a2a_migrator IN SCHEMA a2a_ops
    GRANT USAGE, SELECT, UPDATE ON SEQUENCES TO operations_a2a_runtime;
ALTER DEFAULT PRIVILEGES FOR ROLE operations_workflow_migrator IN SCHEMA workflow_ops
    GRANT USAGE, SELECT, UPDATE ON SEQUENCES TO operations_workflow_runtime;
ALTER DEFAULT PRIVILEGES FOR ROLE operations_gateway_migrator IN SCHEMA gateway_ops
    GRANT USAGE, SELECT, UPDATE ON SEQUENCES TO operations_gateway_runtime;
ALTER DEFAULT PRIVILEGES FOR ROLE operations_audit_migrator IN SCHEMA audit_ops
    GRANT USAGE, SELECT, UPDATE ON SEQUENCES TO operations_audit_publisher;
ALTER DEFAULT PRIVILEGES FOR ROLE operations_artifact_migrator IN SCHEMA artifact_ops
    GRANT USAGE, SELECT, UPDATE ON SEQUENCES TO operations_artifact_runtime;

ALTER ROLE operations_execution_runtime IN DATABASE operations SET search_path = execution_ops, operational_meta, public;
ALTER ROLE operations_agent_runtime IN DATABASE operations SET search_path = agent_ops, operational_meta, public;
ALTER ROLE operations_a2a_runtime IN DATABASE operations SET search_path = a2a_ops, operational_meta, public;
ALTER ROLE operations_workflow_runtime IN DATABASE operations SET search_path = workflow_ops, operational_meta, public;
ALTER ROLE operations_gateway_runtime IN DATABASE operations SET search_path = gateway_ops, operational_meta, public;
ALTER ROLE operations_audit_publisher IN DATABASE operations SET search_path = audit_ops, operational_meta, public;
ALTER ROLE operations_artifact_runtime IN DATABASE operations SET search_path = artifact_ops, operational_meta, public;

