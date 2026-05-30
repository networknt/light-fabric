\set ON_ERROR_STOP on

-- Usage:
--   psql "postgresql://postgres:secret@localhost:5432/configserver" \
--     -v host_id='<host-id>' \
--     -f apps/light-workflow/examples/insurance-claim-demo-reset.sql
--
-- This file does not mutate portal tables. The portal workflow runtime is
-- event-sourced, so demo reset must go through workflow/deleteProcessInfo,
-- which emits ProcessInfoDeletedEvent and lets the projection soft-delete
-- process_info_t.active.
--
-- Use insurance-claim-demo-reset.sh to perform the event-backed reset. Use this
-- SQL file to preview reset candidates and inspect the command payloads.

\echo 'Insurance claim demo reset candidates'
WITH target_processes AS (
  SELECT p.host_id,
         p.process_id,
         p.wf_instance_id,
         d.name AS workflow_name,
         p.status_code,
         p.started_ts
  FROM process_info_t p
  JOIN wf_definition_t d
    ON d.host_id = p.host_id
   AND d.wf_def_id = p.wf_def_id
  WHERE p.host_id = :'host_id'::uuid
    AND p.active = TRUE
    AND d.name IN (
      'insurance-claim-rest-v1',
      'insurance-claim-mcp-v1',
      'insurance-claim-headless-v1'
    )
)
SELECT workflow_name,
       wf_instance_id,
       process_id,
       status_code,
       started_ts,
       jsonb_build_object(
         'host', 'lightapi.net',
         'service', 'workflow',
         'action', 'deleteProcessInfo',
         'version', '0.1.0',
         'data', jsonb_build_object(
           'hostId', host_id,
           'processId', process_id
         )
       ) AS delete_process_info_command
FROM target_processes
ORDER BY started_ts DESC;

\echo 'Remaining active insurance claim demo instances'
SELECT d.name AS workflow_name,
       count(p.process_id) FILTER (WHERE p.active = TRUE) AS active_process_count,
       count(p.process_id) AS total_process_count
FROM wf_definition_t d
LEFT JOIN process_info_t p
  ON p.host_id = d.host_id
 AND p.wf_def_id = d.wf_def_id
WHERE d.host_id = :'host_id'::uuid
  AND d.name IN (
    'insurance-claim-rest-v1',
    'insurance-claim-mcp-v1',
    'insurance-claim-headless-v1'
  )
GROUP BY d.name
ORDER BY d.name;
