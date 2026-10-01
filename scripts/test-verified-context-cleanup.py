#!/usr/bin/env python3
"""Real PostgreSQL cleanup boundaries, restricted to the owned G01 container."""
import json
from pathlib import Path
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
BUNDLE = ROOT / 'crates/operational-store/release/bundle'
TABLES = ['workflow_verified_invocation_t', 'workflow_verified_task_context_t', 'workflow_verified_task_result_t']
UID = "'11111111-1111-4111-8111-111111111111'"
SQL = (ROOT / 'crates/workflow-store/migrations/workflow-postgres/0023_workflow_verified_context_cleanup.sql').read_text()
RESULTS = []


def command(db):
    return ['rtk', 'proxy', 'psql', '-X', '-q', '-At', '-v', 'ON_ERROR_STOP=1', '-d', f'postgres://postgres@127.0.0.1:55433/{db}']


def sql(db, statement, fail=False):
    result = subprocess.run(command(db), input=statement, text=True, capture_output=True)
    if not fail and result.returncode:
        raise AssertionError(result.stderr)
    return result


def create(db, template=None):
    assert db.startswith('g01_cleanup_')
    sql('postgres', f'DROP DATABASE IF EXISTS {db} WITH (FORCE); CREATE DATABASE {db}' + (f' TEMPLATE {template}' if template else '') + ';')


def apply(db, rows, manifest):
    for row in rows:
        sql(db, 'SET search_path TO ' + row['schema'] + ',pg_catalog;\n' + (BUNDLE / row['path']).read_text())
        sql(db, "INSERT INTO operational_meta.operational_schema_migration_t VALUES ('%s','%s','%s','sha256:%s','%s',%s,now());" %
            (row['owner'], row['schema'], row['migrationId'], row['sha256'], manifest['bundleVersion'], manifest['contractGeneration']))


def baseline(db):
    # Table identities and the complete ledger must survive each refusal.
    return (sql(db, "SELECT c.oid||':'||n.nspname||'.'||c.relname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname NOT IN ('pg_catalog','information_schema','pg_toast') AND c.relkind IN ('r','p') ORDER BY 1").stdout,
            sql(db, 'SELECT row_to_json(t) FROM operational_meta.operational_schema_migration_t t ORDER BY migration_owner,migration_id').stdout)


def check(db, expected=None, before=None):
    result = sql(db, SQL, fail=expected is not None)
    if expected:
        assert result.returncode != 0 and expected in result.stderr, result.stderr
        assert baseline(db) == before, 'refusal changed tables or migration ledger'
    else:
        assert result.returncode == 0
        assert sql(db, "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='workflow_ops' AND c.relname=ANY(ARRAY[%s])" % ','.join("'%s'" % table for table in TABLES)).stdout.strip() == '0'
        assert sql(db, "SELECT to_regclass('workflow_ops.workflow_task_timer_t') IS NOT NULL").stdout.strip() == 't'
    return result


def record(case):
    RESULTS.append(case)
    print('PASS ' + case, flush=True)


def seed(table):
    if table.endswith('invocation_t'):
        columns, values = 'host_id,run_id,profile,creator', f"{UID},{UID},'capture-v1','{{}}'"
    elif table.endswith('context_t'):
        columns, values = 'host_id,action_id,run_id,task_id,context', f"{UID},{UID},{UID},{UID},'{{}}'"
    else:
        columns, values = 'host_id,action_id,result', f"{UID},{UID},'{{}}'"
    # Isolate each emptiness guard independently, without fabricating a complete
    # production invocation/permit chain. Only FK triggers are bypassed here.
    return f"SET session_replication_role=replica; INSERT INTO workflow_ops.{table}({columns}) VALUES({values}); SET session_replication_role=origin;"


def writer(db, statements):
    process = subprocess.Popen(command(db), stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    process.stdin.write("BEGIN;" + statements + "SELECT 'g01-ready';\n")
    process.stdin.flush()
    assert process.stdout.readline().strip() == 'g01-ready'
    return process


def close_writer(process, commit=False):
    process.stdin.write(('COMMIT;' if commit else 'ROLLBACK;') + '\n\\q\n')
    process.stdin.flush()
    process.stdin.close()
    assert process.wait(timeout=5) == 0


def main():
    label = subprocess.run(['rtk', 'proxy', 'docker', 'inspect', 'g01-bundle-postgres', '--format', '{{index .Config.Labels "codex.task"}} {{(index (index .NetworkSettings.Ports "5432/tcp") 0).HostIp}} {{(index (index .NetworkSettings.Ports "5432/tcp") 0).HostPort}}'], capture_output=True, text=True)
    assert label.returncode == 0 and label.stdout.strip() == 'G01 127.0.0.1 55433'
    manifest = json.loads((BUNDLE / 'manifest.json').read_text())
    cleanup = next(row for row in manifest['orderedMigrations'] if row['migrationId'] == '0023_workflow_verified_context_cleanup')
    prior = [row for row in manifest['orderedMigrations'] if row != cleanup]
    template = 'g01_cleanup_template'
    create(template)
    apply(template, prior, manifest)
    for table in TABLES:
        assert sql(template, f'SELECT count(*) FROM workflow_ops.{table}').stdout.strip() == '0'
    for row in prior:
        if row['migrationId'] == '0022_workflow_verified_task_context':
            assert row['sha256'] == '80c8a5215aff12f5e4fc106919c60dc39ffc129167485b1ac4f8fb0a7ccbdc23'

    db = 'g01_cleanup_fresh'
    create(db)
    apply(db, manifest['orderedMigrations'], manifest)
    assert sql(db, "SELECT count(*) FROM pg_tables WHERE schemaname='workflow_ops' AND tablename LIKE 'workflow_verified_%'").stdout.strip() == '0'
    assert sql(db, "SELECT count(*) FROM operational_meta.operational_schema_migration_t WHERE migration_id IN ('0022_workflow_verified_task_context','0023_workflow_verified_context_cleanup')").stdout.strip() == '2'
    record('fresh_complete_bundle_0022_then_0023')

    db = 'g01_cleanup_upgrade'
    create(db, template)
    before = baseline(db)
    check(db)
    after = baseline(db)
    assert before[1] == after[1], '0022 applied ledger changed'
    assert set(before[0].splitlines()) - set(after[0].splitlines()) == {line for line in before[0].splitlines() if any(line.endswith('.' + t) for t in TABLES)}
    assert set(after[0].splitlines()) - set(before[0].splitlines()) == set()
    record('upgrade_only_empty_verified_tables_dropped_ledger_unchanged')

    for index, table in enumerate(TABLES):
        db = f'g01_cleanup_rows{index}'
        create(db, template)
        sql(db, seed(table))
        check(db, 'WORKFLOW_VERIFIED_CONTEXT_CLEANUP_NOT_EMPTY', baseline(db))
        assert sql(db, f'SELECT count(*) FROM workflow_ops.{table}').stdout.strip() == '1'
        record('refuses_nonempty_' + table)

    cases = {
        'external_fk': 'CREATE TABLE workflow_ops.g01_dependency(host_id uuid,action_id uuid, FOREIGN KEY(host_id,action_id) REFERENCES workflow_ops.workflow_verified_task_result_t(host_id,action_id));',
        'external_view': 'CREATE VIEW workflow_ops.g01_dependency AS SELECT * FROM workflow_ops.workflow_verified_task_result_t;',
        'rowtype_function': 'CREATE FUNCTION workflow_ops.g01_dependency(p workflow_ops.workflow_verified_task_result_t) RETURNS uuid LANGUAGE SQL AS $$ SELECT p.action_id $$;',
        'string_function': 'CREATE FUNCTION workflow_ops.g01_dependency() RETURNS bigint LANGUAGE plpgsql AS $$ BEGIN RETURN (SELECT count(*) FROM workflow_ops.workflow_verified_task_result_t); END $$;',
        'trigger': 'CREATE FUNCTION workflow_ops.g01_trigger() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$; CREATE TRIGGER g01_dependency BEFORE INSERT ON workflow_ops.workflow_verified_task_result_t FOR EACH ROW EXECUTE FUNCTION workflow_ops.g01_trigger();',
    }
    for case, ddl in cases.items():
        db = 'g01_cleanup_' + case
        create(db, template)
        sql(db, ddl)
        check(db, 'WORKFLOW_VERIFIED_CONTEXT_CLEANUP_DEPENDENCY', baseline(db))
        record('refuses_' + case + '_and_rolls_back')
    for index, table in enumerate(TABLES):
        db = f'g01_cleanup_missing{index}'
        create(db, template)
        sql(db, f'ALTER TABLE workflow_ops.{table} RENAME TO g01_missing;')
        check(db, 'WORKFLOW_VERIFIED_CONTEXT_CLEANUP_MISSING_TABLE', baseline(db))
        record('refuses_missing_' + table)

    db = 'g01_cleanup_locked'
    create(db, template)
    lock = writer(db, 'LOCK TABLE workflow_ops.workflow_verified_task_result_t IN ROW EXCLUSIVE MODE;')
    try:
        started = time.monotonic()
        check(db, 'WORKFLOW_VERIFIED_CONTEXT_CLEANUP_LOCK_TIMEOUT', baseline(db))
        assert 1.8 <= time.monotonic() - started < 5
    finally:
        close_writer(lock)
    # Failed upgrade releases every acquired lock and leaves schema retryable.
    check(db)
    record('held_child_lock_times_out_rolls_back_and_clean_retry_succeeds')

    db = 'g01_cleanup_concurrent'
    create(db, template)
    pending = writer(db, seed(TABLES[2]))
    process = subprocess.Popen(command(db), stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    process.stdin.write(SQL.replace('DO $cleanup$', '/* g01-concurrent-cleanup */ DO $cleanup$'))
    process.stdin.close()
    try:
        started = time.monotonic()
        while sql(db, "SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE '%g01-concurrent-cleanup%'").stdout.strip() == '0':
            assert time.monotonic() - started < 1.5
            time.sleep(0.01)
        close_writer(pending, commit=True)
        expected = baseline(db)
        assert process.wait(timeout=5) != 0
        assert 'WORKFLOW_VERIFIED_CONTEXT_CLEANUP_NOT_EMPTY' in process.stderr.read()
        assert baseline(db) == expected
        assert sql(db, f'SELECT count(*) FROM workflow_ops.{TABLES[2]}').stdout.strip() == '1'
        record('concurrent_child_insert_committed_before_lock_is_rechecked')
    finally:
        if pending.poll() is None:
            close_writer(pending)
        if process.poll() is None:
            process.kill()
            process.wait()
    print(json.dumps({'passed': len(RESULTS), 'failed': 0, 'cases': RESULTS}), flush=True)


if __name__ == '__main__':
    main()
