#!/usr/bin/env python3
"""Local-only observation qualification; packaged Init/service/SDK, owned fakes."""
import argparse
import json
import os
from pathlib import Path
import zipfile

import profiles as p


def fake_script(destination):
    # Reuse the established wire protocol, JWT keys and allowlisted receipts.
    original = Path(p.__file__).with_name('profile_dependencies.py').read_text()
    head, marker, _ = original.partition("if __name__ == '__main__':")
    assert marker
    extra = '''
_original_reply = ProfileHandler.reply
def observed_reply(self, status, value, headers=None):
    headers = dict(headers or {})
    if self.path.startswith('/2015-03-31/functions/'):
        with lock:
            number = len(state['invoke'])
            mode = settings.get('apiIdMode', 'valid')
            api = f'11111111-1111-4111-8111-{number:012d}' if mode == 'valid' else ('PRIVATE_BAD_API_ID' if mode == 'malformed' else None)
            state['invoke'][-1]['invokeApiRequestId'] = api
            # Deliberately different: API IDs are not execution-ID evidence.
            state['invoke'][-1]['fakeExecutionId'] = f'22222222-2222-4222-8222-{number:012d}'
        if api is not None:
            headers['x-amzn-requestid'] = api
    return _original_reply(self, status, value, headers)
ProfileHandler.reply = observed_reply
if __name__ == '__main__':
    ThreadingHTTPServer(('0.0.0.0', 19091), ProfileHandler).serve_forever()
'''
    destination.write_text(head + extra)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--package', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    os.environ['LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS'] = '1'
    checked = p.l2.pack.check_zip(args.package)
    p.write(out, 'package-check.json', checked)
    task = out / 'var-task'
    task.mkdir()
    with zipfile.ZipFile(args.package) as archive:
        (task / 'bootstrap').write_bytes(archive.read('bootstrap'))
    (task / 'bootstrap').chmod(0o755)
    results = []
    for variant in ('jwt', 'security'):
        root = out / variant
        root.mkdir()
        source = root / 'source'
        p.prepare_profile(source, 'jwt', variant)
        p.write(root, 'layer-report.json', p.l2.pack.layer(source, root / 'layer.zip', task / 'bootstrap'))
        p.l2.pack.extract_layer(root / 'layer.zip', root / 'opt')
        fake_script(root / 'owned-dependencies.py')
        instance = p.l2.Rie(task, root / 'opt', root / 'runtime')
        instance.fake_source = root / 'owned-dependencies.py'
        instance.log_level = 'trace'
        with instance:
            secret = b'local-rie-synthetic-key-not-for-any-real-use-0000000000000000'
            p.l2.http(instance.fake_url + '/_configure', {'jwks': {'keys': [
                {'kty': 'oct', 'alg': 'HS256', 'kid': 'local-rie-test', 'k': p.b64(secret)}]}})
            tokens = [p.token('synthetic-A'), p.token('synthetic-B')]
            records = []

            def request(name, token, status=200, path='/hello', body='hello from business', code=None):
                before = instance.state()
                observed = p.request(instance, p.event(path, headers={'authorization': ['Bearer ' + token]}), status, code, body)
                after = instance.state()
                records.append({'case': name, 'response': observed, 'clientSendReceipts': len(after['invoke']) - len(before['invoke']),
                                'jwksServerReceiptDelta': after['jwksFetches'] - before['jwksFetches']})

            request('cold-success', tokens[0])
            request('warm-other-principal', tokens[1])
            p.l2.http(instance.fake_url + '/_configure', {'apiIdMode': 'malformed'})
            request('malformed-api-id-does-not-change-response', tokens[0])
            p.l2.http(instance.fake_url + '/_configure', {'apiIdMode': 'missing'})
            request('absent-api-id-does-not-change-response', tokens[1])
            p.l2.http(instance.fake_url + '/_configure', {'apiIdMode': 'valid', 'backend': {'statusCode': 500, 'body': 'synthetic application error', 'isBase64Encoded': False}})
            request('application-error', tokens[0], 500, body='synthetic application error')
            p.l2.http(instance.fake_url + '/_configure', {'backend': None})
            request('warm-after-application-error', tokens[1])
            request('function-error', tokens[0], 502, '/backend-error', None, 'ERR14014')
            request('warm-after-function-error', tokens[1])
            forged = tokens[0].rsplit('.', 1)[0] + '.' + p.b64(bytes(32))
            tokens.append(forged)
            request('forged-token-zero-dispatch', forged, 401, body=None, code='ERR10000')
            state = instance.state()
            assert len(state['invoke']) == 8 and state['jwksFetches'] == 1 and state['http'] == []
            assert [x['principalId'] for x in state['invoke']] == ['synthetic-A', 'synthetic-B'] * 4
            assert all(x['signed'] and x['logType'] == 'None' for x in state['invoke'])
            assert all(x['invokeApiRequestId'] != x['fakeExecutionId'] for x in state['invoke'])
            logs = instance.logs()
            assert all(token not in logs for token in tokens)
            assert 'PRIVATE_BAD_API_ID' not in logs and 'synthetic business failure' not in logs
            rows = []
            for line in logs.splitlines():
                try:
                    rows.append(json.loads(line))
                except ValueError:
                    pass
            fetches = [x['fields'] for x in rows if x.get('target') == 'light_security::jwks_fetch']
            assert len(fetches) == 2 and all(set(x) == {'event', 'fetch_id'} for x in fetches), fetches
            assert [x['event'] for x in fetches] == ['started', 'completed']
            assert fetches[0]['fetch_id'] == fetches[1]['fetch_id']
            api = [x['fields'] for x in rows if x.get('fields', {}).get('event') == 'invoke_response']
            assert len(api) == 8
            expected_ids = [x['invokeApiRequestId'] if x['invokeApiRequestId'] != 'PRIVATE_BAD_API_ID' else None for x in state['invoke']]
            assert [x.get('backend_invoke_api_request_id') for x in api] == expected_ids, api
            assert all('backend_execution_id' not in x for x in api)
            audits = [x['fields'] for x in rows if x.get('target') == 'audit']
            assert len(audits) == 9 and all(x['finalizations'] == 1 for x in audits)
            assert [x['cold_start'] for x in audits] == [True] + [False] * 8
            assert records[-1]['clientSendReceipts'] == records[-1]['jwksServerReceiptDelta'] == 0
            p.write(root, 'observations.json', records)
            p.write(root, 'diagnostics.json', {'jwksClientOperations': fetches, 'invokeApiResponses': api})
            results.append({'variant': variant, 'serviceRequests': 9, 'sdkResponseEvents': 8,
                            'invokeServerReceipts': 8, 'jwksClientSendOperations': 1, 'jwksServerReceipts': 1,
                            'coldRequests': 1, 'warmRequests': 8, 'allPassed': True})
    p.write(out, 'results.json', {'level': 'local RIE only', 'allPassed': True, 'package': checked,
        'profiles': results, 'awsCalls': 0, 'realTokensUsed': False, 'workerExecutions': 0,
        'limits': ['Client-send operations do not count all possible wire retries',
                   'Invoke API IDs are not proved backend execution IDs; owned fake deliberately differs',
                   'No deployment, release replacement, Validator, Router rewriting or full WP12/WP13 claim']})
    print(json.dumps({'out': str(out), 'serviceRequestsPassed': 18, 'sdkResponseEvents': 16,
                      'invokeReceipts': 16, 'jwksClientSendOperations': 2}))


if __name__ == '__main__':
    main()
