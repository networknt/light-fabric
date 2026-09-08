#!/usr/bin/env python3
"""Live gateway admission qualification. Does not claim browser/rollout completion."""
import argparse
import json
from pathlib import Path
import ssl
import stat
import subprocess
import time
import urllib.error
import urllib.request
import uuid

class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *_args, **_kwargs):
        return None

def secret(path):
    path = Path(path)
    mode = path.stat().st_mode
    if not stat.S_ISREG(mode) or mode & 0o077:
        raise ValueError('Credential files must be regular and owner-only')
    value = path.read_text().strip()
    if not value or len(value) > 65536 or any(c.isspace() for c in value):
        raise ValueError('Invalid credential file')
    return value

def request(opener, endpoint, user, workload, alias, correlation):
    headers = {'Content-Type': 'application/json', 'Authorization': 'Bearer ' + user,
               'X-Correlation-Id': correlation}
    if workload is not None:
        headers['X-Scope-Token'] = 'Bearer ' + workload
    body = json.dumps({'model': alias, 'messages': [{'role': 'user', 'content': 'Reply with OK.'}], 'max_tokens': 8}).encode()
    req = urllib.request.Request(endpoint, body, headers)
    try:
        with opener.open(req, timeout=60) as response:
            payload = json.loads(response.read(1024*1024))
            return response.status, payload
    except urllib.error.HTTPError as error:
        # Never record an error body, which may echo a credential.
        return error.code, None

def audit_rows(correlations):
    # libpq consumes PGHOST/PGDATABASE/PGUSER/PGPASSFILE. No DB credentials in argv.
    literals = ','.join("'" + str(uuid.UUID(value)) + "'" for value in correlations)
    sql = "SELECT coalesce(json_agg(row_to_json(e)), '[]'::json) FROM (SELECT request_id, event_kind, public_alias, generation, snapshot_digest, authorization_context FROM llm_audit_event_t WHERE authorization_context->>'correlationId' IN (" + literals + ")) e"
    result = subprocess.run(['psql', '-X', '-A', '-t', '-v', 'ON_ERROR_STOP=1', '-c', sql], capture_output=True, text=True)
    if result.returncode:
        raise RuntimeError('Audit database query failed')
    return json.loads(result.stdout)

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config', required=True, help='Public deployment expectations JSON')
    parser.add_argument('--user-token-file', required=True)
    parser.add_argument('--workload-token-file', required=True)
    parser.add_argument('--ca-file', required=True)
    parser.add_argument('--report', required=True)
    args = parser.parse_args()
    Path(args.report).write_text(json.dumps({'schemaVersion':1, 'status':'IN_PROGRESS'}) + '\n')
    config = json.loads(Path(args.config).read_text())
    endpoint = config['endpoint']
    from urllib.parse import urlsplit
    url = urlsplit(endpoint)
    if url.scheme != 'https' or not url.hostname or url.username or url.password or url.query or url.fragment or url.path != '/v1/chat/completions':
        raise ValueError('An explicit HTTPS chat-completions endpoint is required')
    for field in ['userId', 'agentDefId', 'workloadClientId', 'hostId']:
        uuid.UUID(config[field])
    if config.get('requireWorkload') is not True:
        raise ValueError('Qualification requires a trusted route configured to require both tokens')
    user, workload = secret(args.user_token_file), secret(args.workload_token_file)
    opener = urllib.request.build_opener(NoRedirect(), urllib.request.HTTPSHandler(context=ssl.create_default_context(cafile=args.ca_file)))
    probes = [('authorized', workload, config['alias'], 200), ('missing-workload', None, config['alias'], 401), ('invalid-workload', 'invalid-phase4-token', config['alias'], 401), ('unassigned-model', workload, config['unassignedAlias'], 404), ('unknown-model', workload, str(uuid.uuid4()), 404)]
    cases = []
    for label, token, alias, expected in probes:
        correlation = str(uuid.uuid4())
        status, body = request(opener, endpoint, user, token, alias, correlation)
        if status != expected:
            raise RuntimeError(f'{label} returned {status}; expected {expected}')
        if label == 'authorized' and (not isinstance(body, dict) or not body.get('choices')):
            raise RuntimeError('Authorized request did not return a model response')
        cases.append({'case':label, 'status':status, 'correlationId':correlation})
    deadline = time.monotonic() + 30
    while True:
        rows = audit_rows([case['correlationId'] for case in cases])
        finished = {row['authorization_context']['correlationId']:row for row in rows if row['event_kind'] == 'request_finished'}
        if all(case['correlationId'] in finished for case in cases):
            break
        if time.monotonic() >= deadline:
            raise RuntimeError('Timed out waiting for persisted audit evidence')
        time.sleep(1)
    for case in cases:
        row = finished[case['correlationId']]
        auth = row['authorization_context']
        if auth.get('userId') != config['userId'] or auth.get('hostId') != config['hostId']:
            raise RuntimeError('Audit user/host attribution differs from deployment expectations')
        if case['case'] == 'authorized':
            expected = {'agentDefId':config['agentDefId'], 'workloadClientId':config['workloadClientId'], 'policyDigest':config['agentPolicyDigest'], 'userAccessDecision':'allowed', 'agentAssignmentDecision':'allowed'}
            if any(auth.get(key) != value for key,value in expected.items()) or row['public_alias'] != config['alias']:
                raise RuntimeError('Audit identity, alias, or policy evidence mismatch')
        elif any(e['request_id'] == row['request_id'] and e['event_kind'] == 'attempt_started' for e in rows):
            raise RuntimeError('Denied request dispatched a provider attempt')
        case['requestId'] = row['request_id']
    report = {'schemaVersion':1, 'status':'GATEWAY_ADMISSION_PASSED', 'browserQualification':'NOT_RUN', 'cases':cases}
    # Report has identifiers/status only; no prompts, responses, bearer tokens, or secrets.
    Path(args.report).write_text(json.dumps(report, indent=2) + '\n')
    print('Gateway admission and persisted audit checks passed. Browser and rollout qualification remain required.')

if __name__ == '__main__':
    try:
        main()
    except (Exception, KeyboardInterrupt):
        # Only our fixed messages are safe; transport exceptions can include URLs.
        print('Qualification failed; no pass report was produced.', file=__import__('sys').stderr)
        raise SystemExit(1)
