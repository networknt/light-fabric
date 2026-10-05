#!/usr/bin/env python3
"""Mandatory isolated Host Tool database lane; rejects missing or skipped tests."""
import argparse
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import time
import uuid

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--output-dir', required=True, type=Path)
args = parser.parse_args()
out = args.output_dir.resolve()
out.mkdir(parents=True, exist_ok=False)
root = Path(__file__).resolve().parent.parent
name = 'host-tool-qualification-' + uuid.uuid4().hex
image = 'postgres@sha256:18cfe3ef5e6815560c98237d6216d1e5119702fb0f3894c8785dd58b8bbe5d73'
def docker(*arguments, input=None):
    return subprocess.run(['docker', *arguments], input=input, text=True, capture_output=True, check=True).stdout
created = False
results = []
try:
    docker('create', '--name', name, '--label', 'purpose=host-tool-access-disposable',
           '--tmpfs', '/var/lib/postgresql/data:rw,size=1073741824', '--publish', '127.0.0.1::5432',
           '--env', 'POSTGRES_PASSWORD=g03-synthetic-only', '--env', 'POSTGRES_DB=g03_fixture', image)
    created = True
    docker('start', name)
    for attempt in range(60):
        # The entrypoint's temporary initialization server has a Unix socket
        # before it finishes creating the database. Require the final TCP server.
        ready = subprocess.run(['docker', 'exec', name, 'pg_isready', '-h', '127.0.0.1', '-U', 'postgres', '-d', 'g03_fixture'], capture_output=True)
        if ready.returncode == 0:
            break
        time.sleep(1)
    else:
        raise RuntimeError('Disposable PostgreSQL fixture did not become ready')
    def sql(text):
        return docker('exec', '-i', name, 'psql', '-X', '-U', 'postgres', '-d', 'g03_fixture', '-v', 'ON_ERROR_STOP=1', '-f', '-', input=text)
    with (out / 'schema.log').open('w') as log:
        log.write(sql("CREATE SCHEMA workflow_ops; CREATE ROLE operations_workflow_runtime LOGIN PASSWORD 'host-access-synthetic'; CREATE ROLE operations_workflow_migrator;"))
        for migration in sorted((root / 'crates/workflow-store/migrations/workflow-postgres').glob('*.sql')):
            log.write(migration.name + '\n' + sql(migration.read_text()))
    env = os.environ.copy()
    database_port = int(docker('port', name, '5432/tcp').strip().rsplit(':', 1)[1])
    env['DATABASE_URL'] = f'postgresql://operations_workflow_runtime:host-access-synthetic@127.0.0.1:{database_port}/g03_fixture'
    env['ADMIN_DATABASE_URL'] = f'postgresql://postgres:g03-synthetic-only@127.0.0.1:{database_port}/g03_fixture'
    env['HOST_ACCESS_HANDOFF'] = str(root / 'apps/light-workflow/tests/fixtures/host-tool-workflow-access-20261004-r1/portal-two-get-handoff.json')
    with socket.socket() as listener:
        listener.bind(('127.0.0.1', 0))
        port = listener.getsockname()[1]
    env.pop('LIGHT_GATEWAY_MCP_URL', None)
    lanes = [
        ('operational', ['--test', 'host_tool_access_postgres'], 4),
        ('publication', ['--lib', 'authenticated_admin_publication_reaches_operational_store'], 1),
        ('token', ['--lib', 'workflow_backed_http_uses_the_shared_run_token_selector'], 1),
        ('recovery', ['--lib', 'host_tool_broad_accepted_run_executes_two_gets_after_disable'], 1),
    ]
    for label, selection, count in lanes:
        lane_env = env.copy()
        if label == 'recovery':
            lane_env['LIGHT_GATEWAY_MCP_URL'] = f'http://127.0.0.1:{port}/mcp'
        command = ['cargo', 'test', '--locked', '-p', 'light-workflow', *selection, '--', '--ignored', '--nocapture', '--test-threads=1']
        result = subprocess.run(command, cwd=root, env=lane_env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        (out / (label + '.log')).write_text(result.stdout)
        summaries = re.findall(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;', result.stdout)
        qualified = result.returncode == 0 and summaries == [(str(count), '0', '0')]
        results.append({'lane': label, 'command': command, 'exit': result.returncode, 'expectedTests': count, 'summaries': summaries, 'qualified': qualified})
        (out / 'results.json').write_text(json.dumps({'qualified': all(r['qualified'] for r in results) and len(results) == len(lanes), 'lanes': results, 'fixture': {'name': name, 'image': image, 'syntheticOnly': True}}, indent=2) + '\n')
        if not qualified:
            raise RuntimeError(f'{label}: mandatory test execution failed; inspect {out / (label + ".log")}')
    print('Qualified: 4 operational plus 3 authenticated/token/recovery PostgreSQL tests; zero failures or ignored tests.')
finally:
    if created:
        docker('rm', '--force', '--volumes', name)
