-- Operational permission is published independently of workflow existence.
CREATE TABLE workflow_ops.tool_workflow_access_t (
 host_id uuid NOT NULL, tool_id uuid NOT NULL, policy_id uuid NOT NULL,
 capability_ref text NOT NULL, tool_version varchar(20) NOT NULL,
 lightapi_digest text NOT NULL CHECK(lightapi_digest ~ '^sha256:[0-9a-f]{64}$'),
 allowed_environments text[] NOT NULL CHECK(cardinality(allowed_environments) BETWEEN 1 AND 16),
 allowed_methods text[] NOT NULL CHECK(cardinality(allowed_methods) BETWEEN 1 AND 6
   AND allowed_methods <@ ARRAY['GET','HEAD','POST','PUT','PATCH','DELETE']::text[]),
 enabled boolean NOT NULL, source_revision bigint NOT NULL CHECK(source_revision>0),
 publication_digest text NOT NULL, actor text NOT NULL,
 published_ts timestamptz NOT NULL DEFAULT now(),
 PRIMARY KEY(host_id,tool_id), UNIQUE(host_id,policy_id)
);
CREATE TABLE workflow_ops.workflow_accepted_tool_authority_t (
 host_id uuid NOT NULL, process_id uuid NOT NULL, tool_id uuid NOT NULL,
 capability_ref text NOT NULL, tool_version text NOT NULL, lightapi_digest text NOT NULL,
 environment text NOT NULL, allowed_methods text[] NOT NULL,
 authorization_source text NOT NULL CHECK(authorization_source IN ('HOST_TOOL','SPECIFIC_GRANT')),
 authority_id uuid NOT NULL, authority_revision bigint NOT NULL,
 authority_digest text NOT NULL,
 binding_id uuid NOT NULL, endpoint_uri text NOT NULL, resolution_document jsonb,
 PRIMARY KEY(host_id,process_id,tool_id,environment),
 FOREIGN KEY(host_id,process_id) REFERENCES workflow_ops.process_info_t(host_id,process_id) ON DELETE CASCADE
);
-- Pinning is immutable. Recovery reads these rows and never refreshes authority.
CREATE TABLE workflow_ops.workflow_tool_authority_acceptance_t (
 host_id uuid NOT NULL, process_id uuid NOT NULL,
 PRIMARY KEY(host_id,process_id),
 FOREIGN KEY(host_id,process_id) REFERENCES workflow_ops.process_info_t(host_id,process_id) ON DELETE CASCADE
);
REVOKE ALL ON workflow_ops.workflow_tool_authority_acceptance_t FROM PUBLIC,operations_workflow_runtime;
GRANT SELECT,INSERT ON workflow_ops.workflow_tool_authority_acceptance_t TO operations_workflow_runtime;
REVOKE UPDATE,DELETE ON workflow_ops.workflow_accepted_tool_authority_t FROM PUBLIC;
REVOKE ALL ON workflow_ops.workflow_accepted_tool_authority_t FROM operations_workflow_runtime;
GRANT SELECT,INSERT ON workflow_ops.workflow_accepted_tool_authority_t TO operations_workflow_runtime;
GRANT SELECT,INSERT,UPDATE ON workflow_ops.tool_workflow_access_t TO operations_workflow_runtime;
