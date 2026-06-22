\set ON_ERROR_STOP on

-- Usage:
--   psql "postgresql://postgres:secret@localhost:5432/configserver" \
--     -v host_id='<host-id>' \
--     -f apps/light-workflow/examples/insurance-claim-demo-queries.sql

\echo 'Insurance claim workflow definitions'
SELECT host_id,
       wf_def_id,
       namespace,
       name,
       version,
       active,
       update_ts
FROM wf_definition_t
WHERE host_id = :'host_id'::uuid
  AND name IN (
    'insurance-claim-rest-v1',
    'insurance-claim-mcp-v1',
    'insurance-claim-headless-v1'
  )
ORDER BY name, update_ts DESC;

\echo 'Latest insurance claim workflow instances'
SELECT p.host_id,
       p.process_id,
       p.wf_instance_id,
       d.name AS workflow_name,
       p.status_code,
       p.started_ts,
       p.completed_ts,
       p.error_info
FROM process_info_t p
JOIN wf_definition_t d
  ON d.host_id = p.host_id
 AND d.wf_def_id = p.wf_def_id
WHERE p.host_id = :'host_id'::uuid
  AND d.name IN (
    'insurance-claim-rest-v1',
    'insurance-claim-mcp-v1',
    'insurance-claim-headless-v1'
  )
ORDER BY p.started_ts DESC
LIMIT 20;

\echo 'Latest task outputs for insurance claim workflows'
SELECT d.name AS workflow_name,
       p.wf_instance_id,
       t.wf_task_id,
       t.task_type,
       t.status_code,
       t.started_ts,
       t.completed_ts,
       t.task_output
FROM task_info_t t
JOIN process_info_t p
  ON p.host_id = t.host_id
 AND p.process_id = t.process_id
JOIN wf_definition_t d
  ON d.host_id = p.host_id
 AND d.wf_def_id = p.wf_def_id
WHERE t.host_id = :'host_id'::uuid
  AND d.name IN (
    'insurance-claim-rest-v1',
    'insurance-claim-mcp-v1',
    'insurance-claim-headless-v1'
  )
ORDER BY t.started_ts DESC
LIMIT 40;

\echo 'Waiting or claimed human tasks'
SELECT d.name AS workflow_name,
       p.wf_instance_id,
       t.wf_task_id,
       t.task_id,
       ta.task_asst_id,
       ta.assignment_type,
       ta.assignment_id,
       ta.category_code,
       ta.reason_code,
       ta.status_code,
       ta.claimed_by,
       ta.active,
       ta.assigned_ts
FROM task_asst_t ta
JOIN task_info_t t
  ON t.host_id = ta.host_id
 AND t.task_id = ta.task_id
JOIN process_info_t p
  ON p.host_id = t.host_id
 AND p.process_id = t.process_id
JOIN wf_definition_t d
  ON d.host_id = p.host_id
 AND d.wf_def_id = p.wf_def_id
WHERE ta.host_id = :'host_id'::uuid
  AND d.name IN (
    'insurance-claim-rest-v1',
    'insurance-claim-mcp-v1',
    'insurance-claim-headless-v1'
  )
ORDER BY ta.assigned_ts DESC
LIMIT 20;

\echo 'Agent task audit payloads'
SELECT d.name AS workflow_name,
       p.wf_instance_id,
       t.wf_task_id,
       t.task_output -> '_agentAudit' AS agent_audit
FROM task_info_t t
JOIN process_info_t p
  ON p.host_id = t.host_id
 AND p.process_id = t.process_id
JOIN wf_definition_t d
  ON d.host_id = p.host_id
 AND d.wf_def_id = p.wf_def_id
WHERE t.host_id = :'host_id'::uuid
  AND d.name IN (
    'insurance-claim-rest-v1',
    'insurance-claim-mcp-v1',
    'insurance-claim-headless-v1'
  )
  AND t.task_output ? '_agentAudit'
ORDER BY t.started_ts DESC
LIMIT 20;

\echo 'Insurance MCP tool catalog entries'
SELECT name,
       implementation_type,
       version,
       active,
       update_ts
FROM tool_t
WHERE host_id = :'host_id'::uuid
  AND name IN (
    'evaluateCoverage',
    'classifyLiability',
    'scoreClaimRisk',
    'listRequiredDocuments',
    'generateCustomerSummary'
  )
ORDER BY name;
