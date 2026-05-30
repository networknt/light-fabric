\set ON_ERROR_STOP on

-- Usage:
--   psql "postgresql://postgres:secret@localhost:5432/configserver" \
--     -v host_id='<host-id>' \
--     -f apps/light-workflow/examples/insurance-claim-demo-reset.sql
--
-- This reset removes runtime process/task/assignment rows for the three
-- insurance claim demo workflow definitions. It leaves workflow definitions,
-- imported agent catalog events, API catalog entries, and event_store_t intact.

\echo 'Deleting insurance claim demo runtime rows'
WITH target_processes AS (
  SELECT p.host_id,
         p.process_id
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
)
DELETE FROM process_info_t p
USING target_processes t
WHERE p.host_id = t.host_id
  AND p.process_id = t.process_id;

\echo 'Remaining insurance claim demo instances'
SELECT d.name AS workflow_name,
       count(*) AS process_count
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
