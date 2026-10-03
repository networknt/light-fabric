-- Owner-run on an isolated, migrated scratch database; no services are started.
-- Fixture inserts bypass unrelated FK/authoring triggers only. CHECKs remain active.
SET LOCAL session_replication_role=replica;
INSERT INTO workflow_ops.wf_definition_t(host_id,wf_def_id,namespace,name,version,definition)
VALUES ('11111111-1111-4111-8111-111111111111','22222222-2222-4222-8222-222222222222','e04','w2','1.0.0','document: {}');
INSERT INTO workflow_ops.process_info_t(host_id,process_id,wf_def_id,wf_instance_id,app_id,process_type,status_code,ex_trigger_ts,expression_profile,definition_snapshot)
VALUES
('11111111-1111-4111-8111-111111111111','33333333-3333-4333-8333-333333333333','22222222-2222-4222-8222-222222222222','w2-legacy','e04','workflow','R',now(),'cel-workflow-v1',NULL),
('11111111-1111-4111-8111-111111111111','44444444-4444-4444-8444-444444444444','22222222-2222-4222-8222-222222222222','w2-v2','e04','workflow','R',now(),'cel-workflow-v2','{"document":{"metadata":{"lightExpressionProfile":"cel-workflow-v2"}}}');
INSERT INTO workflow_ops.task_info_t(host_id,task_id,process_id,wf_instance_id,wf_task_id,task_type,status_code,locked,priority,execution_placement)
VALUES
('11111111-1111-4111-8111-111111111111','55555555-5555-4555-8555-555555555555','33333333-3333-4333-8333-333333333333','w2-legacy','set','set','A','N',0,'host'),
('11111111-1111-4111-8111-111111111111','66666666-6666-4666-8666-666666666666','44444444-4444-4444-8444-444444444444','w2-v2','set','set','A','N',100,'host');
SET LOCAL session_replication_role=origin;
DO $check$
DECLARE bad jsonb; profile text; claimed uuid; fence bigint; owner uuid;
BEGIN
    -- v2 has higher priority: v1 must still select the eligible legacy task.
    SELECT task_id,lease_fencing_token,lease_owner INTO claimed,fence,owner
    FROM workflow_ops.workflow_claim_host_task_v1('77777777-7777-4777-8777-777777777777',30000);
    IF claimed IS DISTINCT FROM '55555555-5555-4555-8555-555555555555'::uuid
       OR fence<>1 OR owner IS DISTINCT FROM '77777777-7777-4777-8777-777777777777'::uuid THEN
        RAISE EXCEPTION 'v1 claim/profile/lease mismatch'; END IF;
    IF EXISTS (SELECT 1 FROM workflow_ops.workflow_claim_host_task_v1('77777777-7777-4777-8777-777777777777',30000)) THEN
        RAISE EXCEPTION 'v1 selected v2'; END IF;
    FOREACH profile IN ARRAY ARRAY['cel-workflow-v1','unknown'] LOOP
        IF EXISTS (SELECT 1 FROM workflow_ops.workflow_claim_host_task_v2('77777777-7777-4777-8777-777777777777',30000,ARRAY[profile])) THEN
            RAISE EXCEPTION 'v2 selected unsupported profile'; END IF;
    END LOOP;
    IF EXISTS (SELECT 1 FROM workflow_ops.workflow_claim_host_task_v2('77777777-7777-4777-8777-777777777777',30000,NULL))
       OR EXISTS (SELECT 1 FROM workflow_ops.workflow_claim_host_task_v2('77777777-7777-4777-8777-777777777777',30000,ARRAY[]::text[])) THEN
        RAISE EXCEPTION 'null/empty capability selected work'; END IF;
    SELECT task_id INTO claimed FROM workflow_ops.workflow_claim_host_task_v2('77777777-7777-4777-8777-777777777777',30000,ARRAY['cel-workflow-v2']);
    IF claimed IS DISTINCT FROM '66666666-6666-4666-8666-666666666666'::uuid THEN
        RAISE EXCEPTION 'v2 did not claim supported work'; END IF;
    -- Both supported profiles, retaining priority order and task-only locking.
    UPDATE workflow_ops.task_info_t SET locked='N',lease_owner=NULL,lease_expires_ts=NULL
      WHERE wf_instance_id IN ('w2-legacy','w2-v2');
    SELECT task_id INTO claimed FROM workflow_ops.workflow_claim_host_task_v2('77777777-7777-4777-8777-777777777777',30000,ARRAY['cel-workflow-v1','cel-workflow-v2']);
    IF claimed IS DISTINCT FROM '66666666-6666-4666-8666-666666666666'::uuid THEN
        RAISE EXCEPTION 'profile filtering changed priority order'; END IF;
    SELECT task_id INTO claimed FROM workflow_ops.workflow_claim_host_task_v2('77777777-7777-4777-8777-777777777777',30000,ARRAY['cel-workflow-v1','cel-workflow-v2']);
    IF claimed IS DISTINCT FROM '55555555-5555-4555-8555-555555555555'::uuid THEN
        RAISE EXCEPTION 'v2 did not support legacy'; END IF;
    -- Copy actual CHECKs/defaults without unrelated relational fixtures.
END
$check$;
CREATE TEMP TABLE process_probe (LIKE workflow_ops.process_info_t INCLUDING DEFAULTS INCLUDING CONSTRAINTS);
INSERT INTO process_probe(host_id,process_id,wf_def_id,wf_instance_id,app_id,process_type,status_code,ex_trigger_ts)
VALUES ('11111111-1111-4111-8111-111111111111','88888888-8888-4888-8888-888888888888','22222222-2222-4222-8222-222222222222','probe','e04','workflow','R',now());
DO $consistency$
DECLARE bad jsonb;
BEGIN
    IF (SELECT expression_profile FROM process_probe)<>'cel-workflow-v1' THEN
        RAISE EXCEPTION 'old-style legacy default changed'; END IF;
    UPDATE process_probe SET definition_snapshot='{"document":{"metadata":{}}}';
    BEGIN
        UPDATE process_probe SET definition_snapshot='{"document":{"metadata":{"lightExpressionProfile":"cel-workflow-v2"}}}';
        RAISE EXCEPTION 'old writer v2 snapshot accepted';
    EXCEPTION WHEN check_violation THEN NULL; END;
    FOR bad IN SELECT value FROM jsonb_array_elements('[null,1,true,[],{},"unknown","cel-workflow-v1"]') LOOP
        BEGIN
            UPDATE process_probe SET definition_snapshot=jsonb_build_object('document',jsonb_build_object('metadata',jsonb_build_object('lightExpressionProfile',bad)));
            RAISE EXCEPTION 'malformed selector accepted as legacy';
        EXCEPTION WHEN check_violation THEN NULL; END;
        BEGIN
            UPDATE process_probe SET expression_profile='cel-workflow-v2', definition_snapshot=jsonb_build_object('document',jsonb_build_object('metadata',jsonb_build_object('lightExpressionProfile',bad)));
            RAISE EXCEPTION 'malformed selector accepted as v2';
        EXCEPTION WHEN check_violation THEN NULL; END;
    END LOOP;
    BEGIN UPDATE process_probe SET expression_profile='cel-workflow-v2',definition_snapshot=NULL;
        RAISE EXCEPTION 'v2 without snapshot accepted'; EXCEPTION WHEN check_violation THEN NULL; END;
    BEGIN UPDATE process_probe SET expression_profile='unknown';
        RAISE EXCEPTION 'unknown profile accepted'; EXCEPTION WHEN check_violation THEN NULL; END;
    UPDATE process_probe SET expression_profile='cel-workflow-v2',definition_snapshot='{"document":{"metadata":{"lightExpressionProfile":"cel-workflow-v2"}}}';
    BEGIN UPDATE process_probe SET expression_profile='cel-workflow-v1';
        RAISE EXCEPTION 'mismatch update accepted'; EXCEPTION WHEN check_violation THEN NULL; END;
    IF (SELECT admission_enabled FROM workflow_ops.workflow_expression_profile_policy_t WHERE profile_id='cel-workflow-v2') IS DISTINCT FROM false THEN
        RAISE EXCEPTION 'admission not seeded OFF'; END IF;
END
$consistency$;
