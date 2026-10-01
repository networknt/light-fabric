BEGIN;

-- Deployment precondition: quiesce admissions and every old Workflow writer.
-- Locks protect the check/drop boundary, not that operational precondition.
SET LOCAL lock_timeout = '2s';

DO $cleanup$
DECLARE
    tables text[] := ARRAY[
        'workflow_verified_invocation_t',
        'workflow_verified_task_context_t',
        'workflow_verified_task_result_t'
    ];
    name text;
    relations oid[];
    row_types oid[];
    rows bigint;
    dependency text;
BEGIN
    FOREACH name IN ARRAY tables LOOP
        IF to_regclass('workflow_ops.' || name) IS NULL THEN
            RAISE EXCEPTION 'WORKFLOW_VERIFIED_CONTEXT_CLEANUP_MISSING_TABLE: %', name;
        END IF;
    END LOOP;

    -- Fixed parent -> child order, matching old-writer acquisition order.
    LOCK TABLE workflow_ops.workflow_verified_invocation_t IN ACCESS EXCLUSIVE MODE;
    LOCK TABLE workflow_ops.workflow_verified_task_context_t IN ACCESS EXCLUSIVE MODE;
    LOCK TABLE workflow_ops.workflow_verified_task_result_t IN ACCESS EXCLUSIVE MODE;

    -- All counts are checked only after all three locks have been acquired.
    FOREACH name IN ARRAY tables LOOP
        EXECUTE format('SELECT count(*) FROM workflow_ops.%I', name) INTO rows;
        IF rows <> 0 THEN
            RAISE EXCEPTION 'WORKFLOW_VERIFIED_CONTEXT_CLEANUP_NOT_EMPTY: % has % rows', name, rows;
        END IF;
    END LOOP;

    SELECT array_agg(c.oid), array_agg(c.reltype)
      INTO relations, row_types
      FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = 'workflow_ops' AND c.relname = ANY(tables);

    -- Permit only table-owned constraints/indexes and the intrinsic row types.
    -- Incoming FKs, views, row-type consumers and other external dependencies
    -- must be reviewed rather than removed implicitly.
    SELECT pg_describe_object(d.classid, d.objid, d.objsubid)
      INTO dependency
      FROM pg_depend d
     WHERE ((d.refclassid = 'pg_class'::regclass AND d.refobjid = ANY(relations))
         OR (d.refclassid = 'pg_type'::regclass AND d.refobjid = ANY(row_types)))
       AND NOT (
           (d.classid = 'pg_constraint'::regclass AND EXISTS (
               SELECT 1 FROM pg_constraint c WHERE c.oid = d.objid AND c.conrelid = ANY(relations)))
        OR (d.classid = 'pg_class'::regclass AND EXISTS (
               SELECT 1 FROM pg_index i WHERE i.indexrelid = d.objid AND i.indrelid = ANY(relations)))
        OR (d.classid = 'pg_class'::regclass AND d.deptype = 'i' AND EXISTS (
               SELECT 1 FROM pg_class c WHERE c.oid = ANY(relations) AND c.reltoastrelid = d.objid))
        OR (d.classid = 'pg_type'::regclass AND d.deptype IN ('i', 'a') AND EXISTS (
               SELECT 1 FROM pg_type t WHERE t.oid = d.objid
                 AND (t.typrelid = ANY(relations) OR t.typelem = ANY(row_types))))
       )
     ORDER BY d.classid, d.objid, d.objsubid LIMIT 1;
    IF dependency IS NOT NULL THEN
        RAISE EXCEPTION 'WORKFLOW_VERIFIED_CONTEXT_CLEANUP_DEPENDENCY: %', dependency;
    END IF;

    -- Function bodies written as strings may have no pg_depend relation edge.
    -- Fail closed on an explicit table-name reference, including dynamic SQL.
    SELECT format('%I.%I', n.nspname, p.proname) INTO dependency
      FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
     WHERE p.prosrc ~* '\mworkflow_verified_(invocation|task_context|task_result)_t\M'
     ORDER BY p.oid LIMIT 1;
    IF dependency IS NOT NULL THEN
        RAISE EXCEPTION 'WORKFLOW_VERIFIED_CONTEXT_CLEANUP_DEPENDENCY: function %', dependency;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_inherits WHERE inhrelid = ANY(relations) OR inhparent = ANY(relations))
       OR EXISTS (SELECT 1 FROM pg_depend WHERE classid = 'pg_class'::regclass
                  AND objid = ANY(relations) AND deptype = 'e') THEN
        RAISE EXCEPTION 'WORKFLOW_VERIFIED_CONTEXT_CLEANUP_DEPENDENCY: inheritance or extension membership';
    END IF;

    DROP TABLE workflow_ops.workflow_verified_task_result_t;
    DROP TABLE workflow_ops.workflow_verified_task_context_t;
    DROP TABLE workflow_ops.workflow_verified_invocation_t;
EXCEPTION
    WHEN lock_not_available THEN
        RAISE EXCEPTION 'WORKFLOW_VERIFIED_CONTEXT_CLEANUP_LOCK_TIMEOUT: quiesce old writers and retry'
            USING ERRCODE = '55P03';
    WHEN undefined_table THEN
        RAISE EXCEPTION 'WORKFLOW_VERIFIED_CONTEXT_CLEANUP_MISSING_TABLE: schema changed during cleanup';
    WHEN dependent_objects_still_exist THEN
        RAISE EXCEPTION 'WORKFLOW_VERIFIED_CONTEXT_CLEANUP_DEPENDENCY: %', SQLERRM;
END
$cleanup$;

COMMIT;
