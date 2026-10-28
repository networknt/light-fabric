#!/usr/bin/env python3
"""Family-scoped L2 checks against the unchanged packaged Rust application."""
import argparse
import base64
import hashlib
import hmac
import json
import os
from pathlib import Path
import time
import zipfile

import run as l2

CLASSES = {
    'proxy': 'proxy.LambdaProxyMiddleware',
    'correlation': 'correlation.CorrelationMiddleware',
    'requestHeader': 'header.RequestHeaderMiddleware',
    'responseHeader': 'header.ResponseHeaderMiddleware',
    'jwt': 'security.JwtVerifyMiddleware',
    'security': 'security.UnifiedSecurityMiddleware',
    'router': 'router.LambdaRouterMiddleware',
    'requestTransformer': 'transformer.RequestTransformerMiddleware',
    'responseTransformer': 'transformer.ResponseTransformerMiddleware',
}
PROFILES = ('headers', 'jwt', 'router', 'transformers')


def write(directory, name, value):
    (directory / name).write_text(json.dumps(value, indent=2) + '\n')


def configuration(directory, chain):
    l2.source(directory)
    write(directory, 'handler.yml', {
        'handlers': [f'com.networknt.aws.lambda.handler.middleware.{CLASSES[a]}@{a}' for a in dict.fromkeys(chain)],
        'defaultHandlers': chain})
    write(directory, 'lambda-proxy.yml', {
        'enabled': True, 'region': 'us-east-1', 'endpointOverride': 'http://127.0.0.1:19091',
        'functions': {'/*@get': 'test-only-business', '/*@post': 'test-only-business'}, 'metricsInjection': False})


def event(path='/hello', method='GET', headers=None, body=None):
    return {'requestContext': {'elb': {'targetGroupArn': 'test-only-target-group'},
                               'authorizer': {'principalId': 'untrusted'}},
            'httpMethod': method, 'path': path, 'multiValueHeaders': headers or {},
            'multiValueQueryStringParameters': {}, 'body': body, 'isBase64Encoded': False}


def request(instance, raw, status, code=None, body=None):
    observed = l2.invoke(instance, raw)
    response = observed['response']
    assert observed['httpStatus'] == 200, observed
    assert response['statusCode'] == status, observed
    if code:
        assert json.loads(response['body'])['code'] == code, observed
    if body is not None:
        assert response['body'] == body, observed
    return observed


def headers_profile(instance):
    records = []
    for correlation in (['first', 'second'], None, ['warm-final']):
        incoming = {'x-unrelated': ['alpha', 'omega'], 'x-remove': ['caller']}
        if correlation:
            incoming['X-Correlation-ID'] = correlation
        records.append(request(instance, event('/hello', headers=incoming), 200))
    records.append(request(instance, event('/backend-error'), 502, 'ERR14014'))
    records.append(request(instance, event('/other'), 200))
    state = instance.state()
    calls = state['invoke']
    assert len(calls) == 5 and state['jwksFetches'] == 0, state
    assert calls[0]['headers']['x-correlation-id'] == 'first', state
    assert calls[0]['multiValueHeaders']['x-correlation-id'] == ['first', 'second'], state
    assert calls[0]['headers']['x-unrelated'] == 'omega', state
    generated = calls[1]['headers']['x-correlation-id']
    assert generated and generated not in ('first', 'second', 'warm-final'), state
    assert calls[2]['headers']['x-correlation-id'] == 'warm-final', state
    assert calls[4]['headers']['x-correlation-id'] not in (generated, 'warm-final', 'first'), state
    for call in calls:
        assert call['headers']['x-request'] == 'configured' and 'x-remove' not in call['headers'], state
    for call in calls[:3]:
        assert call['headers']['x-prefix'] == 'hello', state
    assert 'x-prefix' not in calls[4]['headers'], state
    for record in records:
        response = record['response']['multiValueHeaders']
        assert response['x-response'] == ['configured'] and 'x-business' not in response, record
    assert records[0]['response']['multiValueHeaders']['x-prefix'] == ['hello'], records
    assert 'x-prefix' not in records[-1]['response']['multiValueHeaders'], records
    return records


def b64(value):
    return base64.urlsafe_b64encode(value).decode().rstrip('=')


def token(subject, **overrides):
    # Deliberately public, synthetic HMAC key; never a real credential.
    secret = b'local-rie-synthetic-key-not-for-any-real-use-0000000000000000'
    claims = {'sub': subject, 'user_id': subject, 'client_id': 'synthetic-client',
              'iss': 'test-issuer', 'aud': 'test-audience', 'exp': int(time.time()) + 3600, 'scope': 'read write'}
    claims.update(overrides)
    signing = b64(json.dumps({'alg': 'HS256', 'kid': 'local-rie-test'}).encode()) + '.' + b64(json.dumps(claims).encode())
    return signing + '.' + b64(hmac.new(secret, signing.encode(), hashlib.sha256).digest())


def jwt_profile(instance):
    secret = b'local-rie-synthetic-key-not-for-any-real-use-0000000000000000'
    l2.http(instance.fake_url + '/_configure', {'jwks': {'keys': [
        {'kty': 'oct', 'alg': 'HS256', 'kid': 'local-rie-test', 'k': b64(secret)}]}})
    good = token('synthetic-A')
    invalid = good.rsplit('.', 1)[0] + '.' + b64(bytes(32))
    cases = [(good, 200, None), (None, 401, 'ERR10002'),
             (invalid, 401, 'ERR10000'), (token('wrong', iss='wrong-issuer'), 401, 'ERR10000'),
             (token('expired', exp=int(time.time()) - 3600), 401, 'ERR10001'),
             (token('wrong', aud='wrong-audience'), 401, 'ERR10000'),
             (token('synthetic-B'), 200, None), (good, 200, None)]
    records = []
    for encoded, status, code in cases:
        records.append(request(instance, event(headers={'authorization': ['Bearer ' + encoded]} if encoded else {}), status, code))
    state = instance.state()
    assert len(state['invoke']) == 3 and state['jwksFetches'] == 1, state
    assert [c['principalId'] for c in state['invoke']] == ['synthetic-A', 'synthetic-B', 'synthetic-A'], state
    logs = instance.logs()
    assert all(encoded not in logs for encoded, _, _ in cases if encoded), 'JWT leaked into application logs'
    return records


def router_profile(instance):
    routed = lambda path, method='GET', body=None, service='http-service': event(path, method, {'SeRvIcE_Id': [service]}, body)
    records = [request(instance, routed('/one', body='must-not-forward'), 200, body='{"from":"http"}'),
               request(instance, routed('/two', 'POST', 'business-body'), 200, body='{"from":"http"}'),
               request(instance, event('/fallback'), 200, body='hello from business'),
               request(instance, routed('/one', service='unknown'), 400, 'ERR10089'),
               request(instance, routed('/one', 'HEAD'), 400, 'ERR10089'),
               request(instance, routed('/disconnect'), 400, 'ERR10089'),
               request(instance, routed('/redirect'), 302),
               request(instance, routed('/final'), 200, body='{"from":"http"}'),
               request(instance, event('/fallback'), 200, body='hello from business')]
    state = instance.state()
    assert len(state['invoke']) == 2 and state['jwksFetches'] == 0, state
    calls = state['http']
    assert [c['path'] for c in calls] == ['/base/one', '/base/two', '/base/disconnect', '/base/redirect', '/base/final'], state
    assert calls[0]['body'] == '' and calls[1]['body'] == 'business-body', state
    assert all(not c['serviceIdPresent'] for c in calls), state
    return records


def transformer_profile(instance):
    normal = event('/x', 'POST', {'x-input': ['request-value']}, 'mutate')
    records = [request(instance, normal, 200, body='response-transformed'),
               request(instance, event('/x', 'POST', body='x' * 32768), 500, 'ERR14032'),
               request(instance, normal, 200, body='response-transformed')]
    l2.http(instance.fake_url + '/_configure', {'backend': {'statusCode': 200, 'body': 'x' * 32768}})
    records.append(request(instance, normal, 500, 'ERR14032'))
    l2.http(instance.fake_url + '/_configure', {'backend': None})
    records.append(request(instance, normal, 200, body='response-transformed'))
    records.append(request(instance, event('/other', 'POST', body='untouched'), 200, body='hello from business'))
    state = instance.state()
    assert len(state['invoke']) == 5 and state['jwksFetches'] == 0, state
    assert [c['body'] for c in state['invoke']] == ['request-transformed'] * 4 + ['untouched'], state
    return records


def prepare_profile(source, family, variant):
    chains = {'headers': ['correlation', 'requestHeader', 'proxy', 'responseHeader'],
              'jwt': [variant, 'proxy'], 'router': ['router', 'router', 'proxy'],
              'transformers': ['requestTransformer', 'proxy', 'responseTransformer']}
    configuration(source, chains[family])
    if family == 'headers':
        write(source, 'correlation.yml', {'enabled': True, 'autogenCorrelationID': True})
        write(source, 'header.yml', {'enabled': True,
              'request': {'remove': ['x-remove'], 'update': {'x-request': 'configured'}},
              'response': {'remove': ['x-business'], 'update': {'x-response': 'configured'}},
              'pathPrefixHeader': {'/hello': {'request': {'update': {'x-prefix': 'hello'}},
                                             'response': {'update': {'x-prefix': 'hello'}}}}})
    elif family == 'jwt':
        write(source, 'values.yml', {'light-lambda.stage': 'local-rie-jwt',
              'client.tokenKeyServerUrl': 'http://127.0.0.1:19091', 'client.tokenKeyUri': '/jwks',
              'client.tokenKeyEnableHttp2': False, 'security.enabled': True,
              'unified-security.enabled': True, 'security.issuer': 'test-issuer',
              'security.audience': ['test-audience'], 'security.skipVerifyScopeWithoutSpec': True,
              'unified-security.pathPrefixAuths': [{'prefix': '/', 'jwt': True}]})
    elif family == 'router':
        write(source, 'router.yml', {'enabled': True, 'httpsEnabled': False, 'urlRewriteRules': []})
        write(source, 'direct-registry.yml', {'directUrls': {'http-service': 'http://127.0.0.1:19091/base'}})
    else:
        fixture = l2.ROOT / 'tests/serverless-conformance/fixtures/v1/t/T-02-actual-native-response-input-map-multi-contract-v1.json'
        pair = json.loads(fixture.read_text())
        write(source, 'rule.yml', pair['ruleConfigurationPair']['rust']['rule'])
        for name in ('request-transformer.yml', 'response-transformer.yml'):
            write(source, name, {'enabled': True, 'appliedPathPrefixes': ['/x']})
    return chains[family]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--package', required=True, type=Path)
    parser.add_argument('--out', required=True, type=Path)
    parser.add_argument('--profile', required=True, choices=PROFILES)
    args = parser.parse_args()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    os.environ['LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS'] = '1'
    checked = l2.pack.check_zip(args.package)
    write(out, 'package-check.json', checked)
    task = out / 'var-task'
    task.mkdir()
    with zipfile.ZipFile(args.package) as archive:
        (task / 'bootstrap').write_bytes(archive.read('bootstrap'))
    (task / 'bootstrap').chmod(0o755)
    results = []
    for variant in (('jwt', 'security') if args.profile == 'jwt' else (args.profile,)):
        root = out / variant
        root.mkdir()
        source = root / 'source'
        chain = prepare_profile(source, args.profile, variant)
        report = l2.pack.layer(source, root / 'config-layer.zip', task / 'bootstrap')
        write(root, 'layer-report.json', report)
        manifest = l2.pack.extract_layer(root / 'config-layer.zip', root / 'opt')
        instance = l2.Rie(task, root / 'opt', root / 'runtime')
        instance.fake_source = Path(__file__).with_name('profile_dependencies.py')
        with instance:
            observed = {'headers': headers_profile, 'jwt': jwt_profile, 'router': router_profile,
                        'transformers': transformer_profile}[args.profile](instance)
            state = instance.state()
            assert all(c['signed'] and c['logType'] == 'None' for c in state['invoke']), state
            assert len({c['requestId'] for c in state['invoke']}) == len(state['invoke']), state
            logs = instance.logs()
            assert logs.count('immutable snapshot ready') == 1, logs
            assert manifest['configDigest'] in logs, logs
            audits = []
            for line in logs.splitlines():
                try:
                    row = json.loads(line)
                    if row.get('target') == 'audit':
                        audits.append(row['fields'])
                except ValueError:
                    pass
            assert len(audits) == len(observed) and all(a['finalizations'] == 1 for a in audits), audits
            assert [a['cold_start'] for a in audits] == [True] + [False] * (len(observed) - 1), audits
            results.append({'variant': variant, 'enabledHandlers': list(dict.fromkeys(chain)),
                            'serviceRequests': len(observed), 'admissionOnly': False,
                            'coldRequests': 1, 'warmRequests': len(observed) - 1, 'initSnapshots': 1,
                            'observations': observed, 'invokeCalls': len(state['invoke']),
                            'httpCalls': len(state['http']), 'jwksFetches': state['jwksFetches'], 'pass': True})
        for entry in manifest['files']:
            assert l2.pack.digest((root / 'opt' / entry['path']).read_bytes()) == entry['sha256']
    result = {'level': 'L2 local RIE only', 'profile': args.profile, 'package': checked,
              'cases': results, 'allPassed': True, 'notQualified': ['Validator', 'Router rewriting',
              'TLS JWKS', 'AWS', 'precision child', 'full WP12/WP13']}
    write(out, 'results.json', result)
    print(json.dumps({'out': str(out), 'profile': args.profile, 'passedProfiles': len(results),
                      'serviceRequests': sum(r['serviceRequests'] for r in results)}, indent=2))


if __name__ == '__main__':
    main()
