#!/usr/bin/env python3
"""Owner-only W2 upgrade/rollback/serialization qualification using psql.

No database or service is created. Requires a dedicated e04_w2_* scratch DB
with the PRE-W2 schema, empty workflow fixture tables and superuser authority.
For public, load the recorded portal-db baseline DDL; for workflow_ops, apply
the recorded 2.4.0 bundle under operations_workflow_migrator before running
this script, retaining 0001's default privileges. Never use production.
"""
import argparse
import os
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--schema', choices=['workflow_ops', 'public'], required=True)
    args = parser.parse_args()
    schema = args.schema
    here = Path(__file__).resolve().parent
    if schema == 'workflow_ops':
        root = here.parents[2]
        forward = root / 'crates/workflow-store/migrations/workflow-postgres/0024_workflow_expression_profile.sql'
        rollback = root / 'crates/operational-store/release/bundle/rollback/0024_workflow_expression_profile.sql'
    else:
        root = here.parent
        forward = root / 'patch_20261002_01_workflow_expression_profile.sql'
        rollback = root / 'rollback/rollback_20261002_01_workflow_expression_profile.sql'
    # libpq consumes URL from the environment, never printed or passed in argv.
    env = dict(os.environ)
    env['PGDATABASE'] = env['E04_W2_TEST_DATABASE_URL']
    command = ['rtk', 'proxy', 'psql', '-X', '-qAt', '-v', 'ON_ERROR_STOP=1']

    def sql(statement, expected=None):
        result = subprocess.run(command, input=statement, text=True, capture_output=True, env=env, timeout=15)
        if expected:
            assert result.returncode != 0 and expected in result.stderr, 'expected SQL refusal missing'
        else:
            assert result.returncode == 0, result.stderr
        return result.stdout.strip()

    database = sql('SELECT current_database();')
    assert database.startswith('e04_w2_'), 'dedicated owner-created e04_w2_* database required'
    assert sql('SELECT rolsuper FROM pg_roles WHERE rolname=current_user;') == 't', 'fixture requires superuser'
    assert sql(f"SELECT count(*) FROM information_schema.columns WHERE table_schema='{schema}' AND table_name='process_info_t' AND column_name='expression_profile';") == '0', 'PRE-W2 baseline required'
    tables = ['wf_definition_t', 'wf_definition_version_t', 'process_info_t', 'task_info_t']
    for table in tables:
        assert sql(f'SELECT count(*) FROM {schema}.{table};') == '0', 'empty workflow tables required'
    if schema == 'workflow_ops':
        assert sql("SELECT rolsuper FROM pg_roles WHERE rolname='operations_workflow_migrator';") == 'f', 'actual non-superuser migrator role required'
        # Reproduce the real creation-time ACLs; fail if the scratch setup omitted
        # 0001's defaults. The probe is rolled back, not an application table.
        sql("""BEGIN; SET LOCAL ROLE operations_workflow_migrator;
            CREATE TABLE workflow_ops.e04_w2_default_privilege_probe(id integer);
            DO $defaults$
            DECLARE privilege text;
            BEGIN
                FOREACH privilege IN ARRAY ARRAY['SELECT','INSERT','UPDATE','DELETE'] LOOP
                    IF NOT has_table_privilege('operations_workflow_runtime',
                        'workflow_ops.e04_w2_default_privilege_probe',privilege) THEN
                        RAISE EXCEPTION 'W2_BASELINE_DEFAULT_PRIVILEGES_MISSING: %', privilege;
                    END IF;
                END LOOP;
            END $defaults$; ROLLBACK;""")
        print('PASS actual migrator default ACLs include SELECT/INSERT/UPDATE/DELETE', flush=True)

    def apply_forward(expected=None):
        # SET ROLE precedes the file's BEGIN, so creation-time defaults belong
        # to the configured migrator, even though fixtures use a superuser DSN.
        prefix = 'SET ROLE operations_workflow_migrator;\n' if schema == 'workflow_ops' else ''
        return sql(prefix + forward.read_text(), expected)
    host = "'11111111-1111-4111-8111-111111111111'"
    definition = "'22222222-2222-4222-8222-222222222222'"
    process = "'33333333-3333-4333-8333-333333333333'"
    sql(f"""BEGIN; SET LOCAL session_replication_role=replica;
        INSERT INTO {schema}.wf_definition_t(host_id,wf_def_id,namespace,name,version,definition)
        VALUES({host},{definition},'e04','preflight','1.0.0','document: {{}}');
        INSERT INTO {schema}.process_info_t(host_id,process_id,wf_def_id,wf_instance_id,app_id,process_type,status_code,ex_trigger_ts)
        VALUES({host},{process},{definition},'preflight','e04','workflow','R',now()); COMMIT;""")
    if schema == 'workflow_ops':
        sql(f"INSERT INTO {schema}.wf_definition_version_t(host_id,wf_def_id,version,definition,definition_digest,schema_digest,published_by) VALUES({host},{definition},'1.0.0','document: {{}}','sha256:'||repeat('a',64),'sha256:'||repeat('a',64),'e04');")
    else:
        sql(f"INSERT INTO {schema}.wf_definition_version_t(host_id,wf_def_id,namespace,name,version,definition) VALUES({host},{definition},'e04','preflight','1.0.0','document: {{}}');")

    def fingerprint():
        return '\n'.join(sql(f'SELECT row_to_json(t) FROM {schema}.{table} t ORDER BY 1::text;') for table in tables)

    for table in ['wf_definition_t', 'wf_definition_version_t', 'process_info_t']:
        field = 'definition_snapshot' if table == 'process_info_t' else 'definition'
        collision = "'{\"document\":{\"metadata\":{\"lightExpressionProfile\":null}}}'" if field == 'definition_snapshot' else "'document: {metadata: {lightExpressionProfile: null}}'"
        sql(f'UPDATE {schema}.{table} SET {field}={collision};')
        before = fingerprint()
        apply_forward('WORKFLOW_EXPRESSION_RESERVED_KEY_COLLISION')
        assert fingerprint() == before, 'preflight refusal rewrote stored data'
        assert sql(f"SELECT count(*) FROM information_schema.columns WHERE table_schema='{schema}' AND table_name='process_info_t' AND column_name='expression_profile';") == '0'
        sql(f"UPDATE {schema}.{table} SET {field}=" + ('NULL;' if field == 'definition_snapshot' else "'document: {}';"))
        print('PASS reserved-key collision: ' + table, flush=True)
    # An encoded key OR an unrelated escaped value is unresolved ambiguity,
    # not evidence that the reserved key exists. Both must stop deployment.
    for table in ['wf_definition_t', 'wf_definition_version_t']:
        for source in [r'document: {metadata: {"lightExpression\u0050rofile": null}}',
                       r'document: {name: harmless, metadata: {note: "line\nbreak"}}']:
            sql(f'UPDATE {schema}.{table} SET definition=$yaml${source}$yaml$;')
            before = fingerprint()
            apply_forward('WORKFLOW_EXPRESSION_RESERVED_KEY_AMBIGUITY')
            assert fingerprint() == before, 'ambiguity refusal rewrote stored data'
            assert sql(f"SELECT count(*) FROM information_schema.columns WHERE table_schema='{schema}' AND table_name='process_info_t' AND column_name='expression_profile';") == '0'
        sql(f"UPDATE {schema}.{table} SET definition='document: {{}}';")
    sql(f"BEGIN; SET LOCAL session_replication_role=replica; DELETE FROM {schema}.process_info_t; DELETE FROM {schema}.wf_definition_version_t; DELETE FROM {schema}.wf_definition_t; COMMIT;")
    apply_forward()
    print('PASS forward schema with OFF policy; encoded-key ambiguity rejected', flush=True)
    sql('BEGIN;\n' + (here / 'expression_profile_schema_gate.sql').read_text() + '\nROLLBACK;')
    print('PASS claim v1/v2, lease/priority, consistency/null/malformed/default gates', flush=True)

    # Independent sessions prove SHARE conflicts with UPDATE in both directions.
    def hold(lock):
        child = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env)
        child.stdin.write(f"BEGIN; SELECT 1 FROM {schema}.workflow_expression_profile_policy_t WHERE profile_id='cel-workflow-v2' FOR {lock}; SELECT 'READY';\n")
        child.stdin.flush()
        while child.stdout.readline().strip() != 'READY':
            assert child.poll() is None, 'lock holder failed'
        return child

    for first, second in [('SHARE', 'UPDATE'), ('UPDATE', 'SHARE')]:
        holder = hold(first)
        try:
            sql(f"BEGIN; SET LOCAL lock_timeout='250ms'; SELECT 1 FROM {schema}.workflow_expression_profile_policy_t WHERE profile_id='cel-workflow-v2' FOR {second}; COMMIT;", 'lock timeout')
        finally:
            holder.stdin.write('ROLLBACK;\n\\q\n'); holder.stdin.flush()
            assert holder.wait(timeout=5) == 0
        sql(f"BEGIN; SELECT 1 FROM {schema}.workflow_expression_profile_policy_t WHERE profile_id='cel-workflow-v2' FOR {second}; ROLLBACK;")
    print('PASS FOR SHARE / FOR UPDATE serialization in both directions and retry', flush=True)
    if schema == 'workflow_ops':
        assert sql("""SELECT bool_and(c.relowner='operations_workflow_migrator'::regrole)
            FROM pg_class c WHERE c.oid IN (
                'workflow_ops.workflow_expression_profile_policy_t'::regclass,
                'workflow_ops.workflow_worker_capability_t'::regclass);""") == 't', 'W2 tables must be owned by actual migrator'
        runtime = 'BEGIN; SET LOCAL ROLE operations_workflow_runtime; '
        assert sql(runtime + "SELECT admission_enabled FROM workflow_ops.workflow_expression_profile_policy_t WHERE profile_id='cel-workflow-v2' FOR SHARE; ROLLBACK;") == 'f'
        for denied in [
            "UPDATE workflow_ops.workflow_expression_profile_policy_t SET admission_enabled=true WHERE profile_id='cel-workflow-v2'",
            "INSERT INTO workflow_ops.workflow_expression_profile_policy_t(profile_id) VALUES('forbidden')",
            "DELETE FROM workflow_ops.workflow_expression_profile_policy_t WHERE profile_id='cel-workflow-v2'",
            "UPDATE workflow_ops.workflow_expression_profile_policy_t SET updated_ts=now()",
            "UPDATE workflow_ops.workflow_expression_profile_policy_t SET updated_by='forbidden'",
        ]:
            sql(runtime + denied + '; ROLLBACK;', 'permission denied')
        assert sql("SELECT admission_enabled FROM workflow_ops.workflow_expression_profile_policy_t WHERE profile_id='cel-workflow-v2';") == 'f'
        assert sql("SELECT count(*) FROM workflow_ops.workflow_expression_profile_policy_t;") == '1'
        sql("""DO $acl$
            DECLARE privilege text; col text;
            BEGIN
                IF NOT has_column_privilege('operations_workflow_runtime',
                    'workflow_ops.workflow_expression_profile_policy_t','profile_id','UPDATE') THEN
                    RAISE EXCEPTION 'runtime policy row-lock authority missing';
                END IF;
                FOREACH col IN ARRAY ARRAY['admission_enabled','updated_ts','updated_by'] LOOP
                    IF has_column_privilege('operations_workflow_runtime',
                        'workflow_ops.workflow_expression_profile_policy_t',col,'UPDATE') THEN
                        RAISE EXCEPTION 'runtime policy update authority too broad';
                    END IF;
                END LOOP;
                FOREACH privilege IN ARRAY ARRAY['INSERT','UPDATE','DELETE','TRUNCATE','REFERENCES','TRIGGER'] LOOP
                    IF has_table_privilege('operations_workflow_runtime',
                        'workflow_ops.workflow_expression_profile_policy_t',privilege) THEN
                        RAISE EXCEPTION 'runtime policy table authority too broad: %', privilege;
                    END IF;
                END LOOP;
                FOREACH privilege IN ARRAY ARRAY['SELECT','INSERT','UPDATE'] LOOP
                    IF NOT has_table_privilege('operations_workflow_runtime',
                        'workflow_ops.workflow_worker_capability_t',privilege) THEN
                        RAISE EXCEPTION 'runtime capability authority missing: %', privilege;
                    END IF;
                END LOOP;
                FOREACH privilege IN ARRAY ARRAY['DELETE','TRUNCATE','REFERENCES','TRIGGER'] LOOP
                    IF has_table_privilege('operations_workflow_runtime',
                        'workflow_ops.workflow_worker_capability_t',privilege) THEN
                        RAISE EXCEPTION 'runtime capability authority too broad: %', privilege;
                    END IF;
                END LOOP;
            END $acl$;""")
        assert sql(runtime + """INSERT INTO workflow_ops.workflow_worker_capability_t(
                instance_id,binary_version,supported_profiles,admits_profiles)
                VALUES('99999999-9999-4999-8999-999999999999','w2-acl',ARRAY['cel-workflow-v1'],ARRAY[]::text[]);
            UPDATE workflow_ops.workflow_worker_capability_t SET binary_version='w2-acl-updated',heartbeat_ts=now()
             WHERE instance_id='99999999-9999-4999-8999-999999999999';
            SELECT binary_version FROM workflow_ops.workflow_worker_capability_t
             WHERE instance_id='99999999-9999-4999-8999-999999999999'; ROLLBACK;""") == 'w2-acl-updated'
        sql(runtime + 'DELETE FROM workflow_ops.workflow_worker_capability_t; ROLLBACK;', 'permission denied')
        assert sql('SELECT count(*) FROM workflow_ops.workflow_worker_capability_t;') == '0'
        print('PASS migrator-owned policy read/lock only; policy insert/delete/toggle denied; capability SELECT/INSERT/UPDATE allowed, DELETE denied and ACLs bounded', flush=True)
    # Persist the schema fixture to test active-run rollback refusal.
    sql('BEGIN;\n' + (here / 'expression_profile_schema_gate.sql').read_text() + '\nCOMMIT;')
    sql(f"UPDATE {schema}.workflow_expression_profile_policy_t SET admission_enabled=true WHERE profile_id='cel-workflow-v2';")
    sql(rollback.read_text(), 'WORKFLOW_EXPRESSION_ACTIVE_V2_PROCESS')
    assert sql(f"SELECT admission_enabled FROM {schema}.workflow_expression_profile_policy_t WHERE profile_id='cel-workflow-v2';") == 'f'
    assert sql(f"SELECT to_regprocedure('{schema}.workflow_claim_host_task_v2(uuid,integer,text[])') IS NOT NULL;") == 't'
    print('PASS active v2 rollback refusal leaves OFF and claim v2 intact', flush=True)
    sql(f"UPDATE {schema}.process_info_t SET status_code='C',completed_ts=now();")
    sql(rollback.read_text())
    assert sql(f"SELECT to_regprocedure('{schema}.workflow_claim_host_task_v2(uuid,integer,text[])') IS NULL;") == 't'
    assert sql(f"SELECT count(*) FROM pg_constraint WHERE conrelid='{schema}.process_info_t'::regclass AND conname='process_expression_profile_snapshot_ck';") == '1'
    assert sql(f"SELECT count(*) FROM {schema}.process_info_t WHERE expression_profile='cel-workflow-v2';") == '1'
    sql(f"UPDATE {schema}.task_info_t SET locked='N',lease_owner=NULL,lease_expires_ts=NULL;")
    claimed = sql(f"SELECT task_id FROM {schema}.workflow_claim_host_task_v1('77777777-7777-4777-8777-777777777777',30000);")
    assert claimed == '55555555-5555-4555-8555-555555555555', 'rollback lost legacy-only claim'
    assert sql(f"SELECT count(*) FROM {schema}.workflow_claim_host_task_v1('77777777-7777-4777-8777-777777777777',30000);") == '0'
    print('PASS successful rollback retains historical v2 marker, CHECK, disabled policy and legacy-only v1', flush=True)
    print('DATABASE QUALIFICATION COMPLETE: owner-run evidence only', flush=True)


if __name__ == '__main__':
    main()
