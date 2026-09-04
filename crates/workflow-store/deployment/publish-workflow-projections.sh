#!/usr/bin/env bash
set -euo pipefail

# Deployment-time publisher for co-located development databases. Production
# publishers must deliver the same Workflow-owned projection contract through
# an authenticated channel.

database_user="${POSTGRES_USER:-postgres}"
source_database="${WORKFLOW_PROJECTION_SOURCE_DATABASE:-configserver}"
target_database="${WORKFLOW_PROJECTION_TARGET_DATABASE:-operations}"
database_host="${OPERATIONAL_DATABASE_HOST:-postgres}"
database_port="${OPERATIONAL_DATABASE_PORT:-5432}"
minimum_definitions="${WORKFLOW_PROJECTION_MINIMUM_DEFINITIONS:-1}"
minimum_bindings="${WORKFLOW_PROJECTION_MINIMUM_BINDINGS:-1}"
minimum_endpoints="${WORKFLOW_PROJECTION_MINIMUM_ENDPOINTS:-0}"

fail() {
  echo "workflow-projection-publisher: $*" >&2
  exit 1
}

[[ "$source_database" =~ ^[a-z][a-z0-9_]{0,62}$ ]] || fail "invalid source database"
[[ "$target_database" =~ ^[a-z][a-z0-9_]{0,62}$ ]] || fail "invalid target database"
[[ "$database_host" =~ ^[A-Za-z0-9._-]+$ ]] || fail "invalid database host"
[[ "$database_port" =~ ^[0-9]{1,5}$ ]] || fail "invalid database port"
[[ "$minimum_definitions" =~ ^[0-9]+$ ]] || fail "invalid minimum definition count"
[[ "$minimum_bindings" =~ ^[0-9]+$ ]] || fail "invalid minimum binding count"
[[ "$minimum_endpoints" =~ ^[0-9]+$ ]] || fail "invalid minimum endpoint count"

source_ready="$(psql -U "$database_user" -d "$source_database" -X -tAc \
  "SELECT to_regclass('configserver.wf_definition_t') IS NOT NULL
      AND to_regclass('configserver.workflow_tool_binding_t') IS NOT NULL
      AND to_regclass('configserver.workflow_tool_grant_t') IS NOT NULL")"
[[ "$source_ready" == "t" ]] || fail "source Workflow projections are unavailable"

target_ready="$(psql -U "$database_user" -d "$target_database" -X -tAc \
  "SELECT to_regclass('workflow_ops.wf_definition_t') IS NOT NULL
      AND EXISTS (
        SELECT 1 FROM operational_meta.operational_schema_migration_t
         WHERE migration_owner='workflow-store'
           AND schema_name='workflow_ops'
           AND migration_id='0005_workflow_catalog_projection'
      ) AND EXISTS (
        SELECT 1 FROM operational_meta.operational_schema_migration_t
         WHERE migration_owner='workflow-store'
           AND schema_name='workflow_ops'
           AND migration_id='0006_workflow_endpoint_resolution'
      )")"
[[ "$target_ready" == "t" ]] || fail "target Workflow projection schema is unavailable"

identity_count="$(psql -U "$database_user" -d "$target_database" -X -tAc \
  "SELECT count(*) FROM operational_meta.operational_database_identity_t
    WHERE singleton AND scope_root_id IS NOT NULL")"
[[ "$identity_count" == "1" ]] || fail "target operational database identity is unavailable or ambiguous"

psql -U "$database_user" -d "$target_database" -X --quiet --set=ON_ERROR_STOP=1 \
  --set=source_database="$source_database" \
  --set=source_host="$database_host" \
  --set=source_port="$database_port" \
  --set=source_user="$database_user" \
  --set=source_password="${PGPASSWORD:-}" <<'SQL'
BEGIN;
CREATE EXTENSION IF NOT EXISTS postgres_fdw;
DROP SCHEMA IF EXISTS workflow_projection_source CASCADE;
DROP SERVER IF EXISTS workflow_projection_source CASCADE;
CREATE SCHEMA workflow_projection_source;
SELECT format(
  'CREATE SERVER workflow_projection_source FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host %L, port %L, dbname %L)',
  :'source_host', :'source_port', :'source_database'
) \gexec
SELECT format(
  'CREATE USER MAPPING FOR CURRENT_USER SERVER workflow_projection_source OPTIONS (user %L, password %L)',
  :'source_user', :'source_password'
) \gexec
IMPORT FOREIGN SCHEMA configserver LIMIT TO (
  wf_definition_t,
  workflow_endpoint_target_t,
  workflow_execution_policy_t,
  workflow_tool_binding_t,
  workflow_tool_dependency_t,
  workflow_tool_grant_t,
  tool_t,
  api_t,
  api_version_t,
  api_endpoint_t,
  api_endpoint_scope_t,
  api_endpoint_rule_t,
  role_permission_t,
  group_permission_t,
  user_permission_t,
  position_permission_t,
  attribute_permission_t
) FROM SERVER workflow_projection_source INTO workflow_projection_source;

SELECT scope_root_id AS projection_host_id
  FROM operational_meta.operational_database_identity_t
 WHERE singleton \gset

UPDATE workflow_ops.wf_definition_t SET active=FALSE
 WHERE host_id=:'projection_host_id'::uuid;
UPDATE workflow_ops.workflow_tool_binding_t SET active=FALSE
 WHERE host_id=:'projection_host_id'::uuid;
UPDATE workflow_ops.workflow_tool_dependency_t SET active=FALSE
 WHERE host_id=:'projection_host_id'::uuid;
UPDATE workflow_ops.workflow_tool_grant_t SET active=FALSE
 WHERE host_id=:'projection_host_id'::uuid;
UPDATE workflow_ops.workflow_endpoint_target_t SET active=FALSE
 WHERE host_id=:'projection_host_id'::uuid;

INSERT INTO workflow_ops.wf_definition_t(
  host_id,wf_def_id,namespace,name,version,definition,lifecycle_status,
  catalog_visible,owner_user_id,owner_position_id,aggregate_version,active,
  update_ts,update_user)
SELECT host_id,wf_def_id,namespace,name,version,definition,lifecycle_status,
       catalog_visible,owner_user_id,owner_position_id,aggregate_version,active,
       update_ts,update_user
  FROM workflow_projection_source.wf_definition_t
 WHERE host_id=:'projection_host_id'::uuid
ON CONFLICT(host_id,wf_def_id) DO UPDATE SET
  namespace=EXCLUDED.namespace,name=EXCLUDED.name,version=EXCLUDED.version,
  definition=EXCLUDED.definition,lifecycle_status=EXCLUDED.lifecycle_status,
  catalog_visible=EXCLUDED.catalog_visible,owner_user_id=EXCLUDED.owner_user_id,
  owner_position_id=EXCLUDED.owner_position_id,
  aggregate_version=EXCLUDED.aggregate_version,active=EXCLUDED.active,
  update_ts=EXCLUDED.update_ts,update_user=EXCLUDED.update_user;

INSERT INTO workflow_ops.workflow_tool_binding_t(
  host_id,binding_id,tool_id,wf_def_id,workflow_version,definition_digest,
  schema_digest,invocation_mode,sync_wait_ms,total_deadline_ms,execution_class,
  result_text_mode,idempotency_policy,delegation_policy,response_policy_digest,
  runtime_bounds,aggregate_version,active,update_user,update_ts,policy_digest,
  tool_name)
SELECT b.host_id,b.binding_id,b.tool_id,b.wf_def_id,b.workflow_version,
       b.definition_digest,b.schema_digest,b.invocation_mode,b.sync_wait_ms,
       b.total_deadline_ms,b.execution_class,b.result_text_mode,
       b.idempotency_policy,b.delegation_policy,b.response_policy_digest,
       b.runtime_bounds,b.aggregate_version,(b.active AND t.active),b.update_user,b.update_ts,
       b.policy_digest,t.name
  FROM workflow_projection_source.workflow_tool_binding_t b
  JOIN workflow_projection_source.tool_t t
    ON t.host_id=b.host_id AND t.tool_id=b.tool_id
 WHERE b.host_id=:'projection_host_id'::uuid
ON CONFLICT(host_id,binding_id) DO UPDATE SET
  tool_id=EXCLUDED.tool_id,wf_def_id=EXCLUDED.wf_def_id,
  workflow_version=EXCLUDED.workflow_version,
  definition_digest=EXCLUDED.definition_digest,schema_digest=EXCLUDED.schema_digest,
  invocation_mode=EXCLUDED.invocation_mode,sync_wait_ms=EXCLUDED.sync_wait_ms,
  total_deadline_ms=EXCLUDED.total_deadline_ms,
  execution_class=EXCLUDED.execution_class,result_text_mode=EXCLUDED.result_text_mode,
  idempotency_policy=EXCLUDED.idempotency_policy,
  delegation_policy=EXCLUDED.delegation_policy,
  response_policy_digest=EXCLUDED.response_policy_digest,
  runtime_bounds=EXCLUDED.runtime_bounds,aggregate_version=EXCLUDED.aggregate_version,
  active=EXCLUDED.active,update_user=EXCLUDED.update_user,
  update_ts=EXCLUDED.update_ts,policy_digest=EXCLUDED.policy_digest,
  tool_name=EXCLUDED.tool_name;

INSERT INTO workflow_ops.workflow_tool_dependency_t
SELECT * FROM workflow_projection_source.workflow_tool_dependency_t
 WHERE host_id=:'projection_host_id'::uuid
ON CONFLICT(host_id,outer_binding_id,nested_tool_id,nested_tool_version) DO UPDATE SET
  contract_digest=EXCLUDED.contract_digest,
  compatibility_policy=EXCLUDED.compatibility_policy,
  authorization_tool_name=EXCLUDED.authorization_tool_name,
  authorization_endpoint_key=EXCLUDED.authorization_endpoint_key,
  authorization_policy_digest=EXCLUDED.authorization_policy_digest,
  lifecycle_status=EXCLUDED.lifecycle_status,dispatch_target=EXCLUDED.dispatch_target,
  retention_until=EXCLUDED.retention_until,active=EXCLUDED.active,
  update_user=EXCLUDED.update_user,update_ts=EXCLUDED.update_ts;

INSERT INTO workflow_ops.workflow_tool_grant_t
SELECT * FROM workflow_projection_source.workflow_tool_grant_t
 WHERE host_id=:'projection_host_id'::uuid
ON CONFLICT(host_id,grant_id) DO UPDATE SET
  tool_id=EXCLUDED.tool_id,wf_def_id=EXCLUDED.wf_def_id,
  tool_version=EXCLUDED.tool_version,lightapi_digest=EXCLUDED.lightapi_digest,
  allowed_environments=EXCLUDED.allowed_environments,
  aggregate_version=EXCLUDED.aggregate_version,active=EXCLUDED.active,
  update_user=EXCLUDED.update_user,update_ts=EXCLUDED.update_ts;

INSERT INTO workflow_ops.workflow_endpoint_target_t(
  host_id,binding_id,endpoint_ref,endpoint_uri,allowed_methods,
  authorization_policy_digest,active,update_user,update_ts,resolution_document)
SELECT source.host_id,source.binding_id,source.endpoint_ref,source.endpoint_uri,
       source.allowed_methods,source.authorization_policy_digest,
       source.active AND binding.active,source.update_user,source.update_ts,NULL
  FROM workflow_projection_source.workflow_endpoint_target_t source
  JOIN workflow_ops.workflow_tool_binding_t binding
    ON binding.host_id=source.host_id AND binding.binding_id=source.binding_id
 WHERE source.host_id=:'projection_host_id'::uuid
ON CONFLICT(host_id,binding_id,endpoint_ref) DO UPDATE SET
  endpoint_uri=EXCLUDED.endpoint_uri,allowed_methods=EXCLUDED.allowed_methods,
  authorization_policy_digest=EXCLUDED.authorization_policy_digest,
  active=EXCLUDED.active,update_user=EXCLUDED.update_user,
  update_ts=EXCLUDED.update_ts,resolution_document=EXCLUDED.resolution_document;

-- Compile eligible logical LightAPI grants into the Workflow-owned endpoint
-- projection. The runtime receives only the pinned target, never Portal tables.
INSERT INTO workflow_ops.workflow_endpoint_target_t(
  host_id,binding_id,endpoint_ref,endpoint_uri,allowed_methods,
  authorization_policy_digest,active,update_user,update_ts,resolution_document)
SELECT DISTINCT ON (b.host_id,b.binding_id,t.capability_ref)
       b.host_id,b.binding_id,t.capability_ref,
       av.protocol || '://' || av.target_host,
       ARRAY[upper(e.http_method)],b.policy_digest,TRUE,
       b.update_user,GREATEST(b.update_ts,g.update_ts,t.update_ts,e.update_ts,av.update_ts,a.update_ts),
       jsonb_strip_nulls(jsonb_build_object(
         'operations',jsonb_build_object(callable_operation.key,callable_operation.value),
         'environments',t.lightapi_document->'environments',
         'authentications',t.lightapi_document->'authentications'))
  FROM workflow_projection_source.workflow_tool_binding_t b
  JOIN workflow_ops.workflow_tool_binding_t projected_binding
    ON projected_binding.host_id=b.host_id
   AND projected_binding.binding_id=b.binding_id
   AND projected_binding.active
  JOIN workflow_projection_source.workflow_tool_grant_t g
    ON g.host_id=b.host_id AND g.wf_def_id=b.wf_def_id
  JOIN workflow_projection_source.tool_t t
    ON t.host_id=g.host_id AND t.tool_id=g.tool_id
  JOIN workflow_projection_source.api_endpoint_t e
    ON e.host_id=t.host_id AND e.endpoint_id=t.endpoint_id
  JOIN workflow_projection_source.api_version_t av
    ON av.host_id=e.host_id AND av.api_version_id=e.api_version_id
  JOIN workflow_projection_source.api_t a
    ON a.host_id=av.host_id AND a.api_id=av.api_id
  JOIN LATERAL (
    SELECT operation.key,operation.value
      FROM jsonb_each(t.lightapi_document->'operations') operation
     WHERE operation.value->>'endpointId'=t.capability_ref
       AND operation.value->>'protocol'='http'
       AND lower(operation.value->>'method')=lower(e.http_method)
       AND COALESCE(operation.value->>'lifecycle','active')='active'
       AND NULLIF(btrim(operation.value->>'endpoint'),'') IS NOT NULL
       AND (
         operation.value->'authentication'->>'type'='none'
         OR (
           jsonb_typeof(operation.value->'authentication')='string'
           AND t.lightapi_document->'authentications'
                 ->(operation.value->>'authentication')->>'type'='none'
         )
       )
     ORDER BY operation.key
     LIMIT 1
  ) callable_operation ON TRUE
 WHERE b.host_id=:'projection_host_id'::uuid
   AND b.active AND g.active AND t.active AND e.active AND av.active AND a.active
   AND t.version=g.tool_version AND t.lightapi_digest=g.lightapi_digest
   AND t.lightapi_validation_status='VALID' AND t.lifecycle_status='active'
   AND e.lifecycle_status='active' AND av.target_host IS NOT NULL
   AND av.protocol IN ('http','https')
   AND btrim(av.target_host)<>'' AND av.target_host NOT LIKE '%@%'
   AND upper(e.http_method)=ANY(ARRAY['GET','HEAD','POST','PUT','PATCH','DELETE'])
   AND NOT EXISTS (SELECT 1 FROM workflow_projection_source.api_endpoint_scope_t x
                    WHERE x.host_id=e.host_id AND x.endpoint_id=e.endpoint_id AND x.active)
   AND NOT EXISTS (SELECT 1 FROM workflow_projection_source.api_endpoint_rule_t x
                    WHERE x.host_id=e.host_id AND x.endpoint_id=e.endpoint_id AND x.active)
   AND NOT EXISTS (SELECT 1 FROM workflow_projection_source.role_permission_t x
                    WHERE x.host_id=e.host_id AND x.endpoint_id=e.endpoint_id AND x.active)
   AND NOT EXISTS (SELECT 1 FROM workflow_projection_source.group_permission_t x
                    WHERE x.host_id=e.host_id AND x.endpoint_id=e.endpoint_id AND x.active)
   AND NOT EXISTS (SELECT 1 FROM workflow_projection_source.user_permission_t x
                    WHERE x.host_id=e.host_id AND x.endpoint_id=e.endpoint_id AND x.active)
   AND NOT EXISTS (SELECT 1 FROM workflow_projection_source.position_permission_t x
                    WHERE x.host_id=e.host_id AND x.endpoint_id=e.endpoint_id AND x.active)
   AND NOT EXISTS (SELECT 1 FROM workflow_projection_source.attribute_permission_t x
                    WHERE x.host_id=e.host_id AND x.endpoint_id=e.endpoint_id AND x.active)
 ORDER BY b.host_id,b.binding_id,t.capability_ref,g.update_ts DESC,g.grant_id
ON CONFLICT(host_id,binding_id,endpoint_ref) DO UPDATE SET
  endpoint_uri=EXCLUDED.endpoint_uri,allowed_methods=EXCLUDED.allowed_methods,
  authorization_policy_digest=EXCLUDED.authorization_policy_digest,
  active=EXCLUDED.active,update_user=EXCLUDED.update_user,
  update_ts=EXCLUDED.update_ts,resolution_document=EXCLUDED.resolution_document;

INSERT INTO workflow_ops.workflow_execution_policy_t
SELECT * FROM workflow_projection_source.workflow_execution_policy_t
 WHERE host_id=:'projection_host_id'::uuid
ON CONFLICT(policy_snapshot_id) DO UPDATE SET
  host_id=EXCLUDED.host_id,tenant_id=EXCLUDED.tenant_id,
  definition_digest=EXCLUDED.definition_digest,profile_id=EXCLUDED.profile_id,
  profile_version=EXCLUDED.profile_version,resolved_policy=EXCLUDED.resolved_policy,
  policy_digest=EXCLUDED.policy_digest,source=EXCLUDED.source,
  created_by=EXCLUDED.created_by,created_ts=EXCLUDED.created_ts;

DROP SCHEMA workflow_projection_source CASCADE;
DROP SERVER workflow_projection_source CASCADE;
COMMIT;
SQL

binding_count="$(psql -U "$database_user" -d "$target_database" -X -tAc \
  "SELECT count(*) FROM workflow_ops.workflow_tool_binding_t WHERE active AND tool_name IS NOT NULL")"
definition_count="$(psql -U "$database_user" -d "$target_database" -X -tAc \
  "SELECT count(*) FROM workflow_ops.wf_definition_t WHERE active")"
endpoint_count="$(psql -U "$database_user" -d "$target_database" -X -tAc \
  "SELECT count(*) FROM workflow_ops.workflow_endpoint_target_t WHERE active")"
(( definition_count >= minimum_definitions )) || fail "only $definition_count active definitions were published"
(( binding_count >= minimum_bindings )) || fail "only $binding_count active tool bindings were published"
(( endpoint_count >= minimum_endpoints )) || fail "only $endpoint_count active endpoint targets were published"
echo "Published $definition_count active Workflow definitions and $binding_count active tool bindings into $target_database."
