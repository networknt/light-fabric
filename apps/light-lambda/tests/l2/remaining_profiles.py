#!/usr/bin/env python3
"""L2 service qualification of remaining admitted aliases using the frozen ZIP."""
import argparse
import base64
import json
import os
from pathlib import Path
import time
import zipfile
import profiles as p

l2 = p.l2
PREFIX = 'com.networknt.aws.lambda.handler.'
CLASSES = {a: PREFIX + 'middleware.' + c for a, c in p.CLASSES.items()}
CLASSES.update({a: PREFIX + c for a, c in {
    'metrics': 'middleware.metrics.APMMetricsMiddleware', 'limit': 'middleware.limit.LimitMiddleware',
    'traceability': 'middleware.traceability.TraceabilityMiddleware',
    'requestCors': 'middleware.cors.RequestCorsMiddleware', 'responseCors': 'middleware.cors.ResponseCorsMiddleware',
    'audit': 'middleware.audit.AuditMiddleware', 'token': 'middleware.token.TokenMiddleware',
    'specification': 'middleware.specification.OpenApiMiddleware', 'basic': 'middleware.security.BasicAuthMiddleware',
    'apikey': 'middleware.security.ApiKeyMiddleware', 'sanitizer': 'middleware.sanitizer.SanitizerMiddleware',
    'health': 'health.HealthCheckHandler', 'info': 'info.ServerInfoHandler', 'getLogger': 'logger.LoggerGetHandler',
}.items()})
FAMILIES = ('cors', 'factors', 'limit', 'telemetry', 'token', 'specification', 'sanitizer', 'admin')
QUALIFIED = {'cors': ['requestCors', 'responseCors'], 'basic': ['basic'], 'apikey': ['apikey'],
             'limit': ['limit'], 'telemetry': ['metrics', 'audit'], 'token': ['token'],
             'specification': ['specification'], 'sanitizer': ['sanitizer'],
             'admin': ['health', 'info', 'getLogger']}


def chain_file(source, chain, paths=None, extra=()):
    p.write(source, 'handler.yml', {'handlers': [CLASSES[a] + '@' + a for a in dict.fromkeys([*chain, *extra])],
                                   'defaultHandlers': chain, 'paths': paths or []})


def prepare(source, family, variant):
    l2.source(source)
    values = {'light-lambda.stage': 'local-l2-remaining', 'security.enabled': False,
              'unified-security.enabled': False, 'client.tokenKeyServerUrl': 'http://127.0.0.1:19091',
              'client.tokenKeyUri': '/jwks', 'client.tokenKeyEnableHttp2': False}
    chains = {'cors': ['requestCors', 'proxy', 'responseCors'], 'factors': [variant, 'proxy'],
              'limit': ['limit', 'proxy'], 'telemetry': ['metrics', 'traceability', 'audit', 'proxy'],
              'token': ['token', 'proxy'], 'specification': ['specification', 'jwt', 'proxy'],
              'sanitizer': ['sanitizer', 'proxy'], 'admin': ['proxy']}
    chain = chains[family]
    chain_file(source, chain)
    functions = {'/*@get': 'test-only-business', '/*@post': 'test-only-business'}
    if family in ('admin', 'specification'):
        values.update({'security.enabled': True, 'security.issuer': 'test-issuer',
                       'security.audience': ['test-audience'], 'security.skipVerifyScopeWithoutSpec': True})
    if family == 'cors':
        functions['/*@options'] = 'test-only-business'
        p.write(source, 'cors.yml', {'enabled': True, 'allowedOrigins': ['https://test.invalid'],
                                     'allowedMethods': ['GET', 'POST']})
    elif family == 'factors':
        if variant == 'basic':
            p.write(source, 'basic-auth.yml', {'enabled': True, 'users': [
                {'username': 'synthetic-user', 'password': 'synthetic-password', 'paths': ['/allowed']}]})
        else:
            p.write(source, 'apikey.yml', {'enabled': True, 'pathPrefixAuths': [
                {'pathPrefix': '/allowed', 'headerName': 'x-test-key', 'apiKey': 'synthetic-api-key'}]})
    elif family == 'limit':
        p.write(source, 'limit.yml', {'enabled': True, 'key': 'address', 'rateLimit': '1/h',
                                     'address': {'198.51.100.7': '1/h', '198.51.100.8': '1/h'}})
    elif family == 'telemetry':
        p.write(source, 'metrics.yml', {'enabled': True})
        p.write(source, 'traceability.yml', {'enabled': True})
        p.write(source, 'audit.yml', {'enabled': True, 'auditOnError': False, 'mask': True,
                'statusCode': True, 'responseTime': False, 'headers': ['x-test-mask'],
                'audit': ['serviceId'], 'timestampFormat': "'synthetic-time'"})
        p.write(source, 'mask.yml', {'regex': {'requestHeader': {'x-test-mask': 'id=(.+)'}}})
        p.write(source, 'lambda-app.yml', {'lambdaAppId': 'trusted-l2-service'})
        functions = {'/pets/{id}@get': 'test-only-business', '/backend-error@get': 'test-only-business'}
    elif family == 'token':
        p.write(source, 'token.yml', {'enabled': True, 'appliedPathPrefixes': ['/x']})
        values.update({'client.tokenServerUrl': 'http://127.0.0.1:19091', 'client.tokenCcUri': '/token',
                       'client.tokenCcClientId': 'synthetic-client', 'client.tokenCcClientSecret': 'synthetic-oauth-secret',
                       'client.tokenEnableHttp2': False, 'client.tokenRenewBeforeExpired': 0,
                       'client.expiredRefreshRetryDelay': 0, 'client.earlyRefreshRetryDelay': 0,
                       'client.maxRequestRetry': 0,
                       'cache.caches': [{'cacheName': 'token', 'maxSize': 100, 'expiryInMinutes': 15}]})
    elif family == 'specification':
        values['security.skipVerifyScopeWithoutSpec'] = False
        p.write(source, 'openapi-handler.yml', {'enabled': True})
        p.write(source, 'openapi.yml', {'openapi': '3.1.0', 'servers': [{'url': 'https://never-contact.invalid/api'}],
                'components': {'securitySchemes': {'z': {'type': 'oauth2'}, 'a': {'type': 'oauth2'}}},
                'paths': {'/write': {'get': {'security': [{'z': ['write'], 'a': ['read']}]}},
                          '/read': {'get': {'security': [{'a': ['read']}]}}}})
    elif family == 'sanitizer':
        values.update({'sanitizer.enabled': True, 'sanitizer.bodyEncoder': 'html', 'sanitizer.headerEnabled': False})
    elif family == 'admin':
        values.update({'health.enabled': True, 'info.enableServerInfo': True, 'logger.enabled': True})
        paths = [{'path': '/health', 'method': 'get', 'exec': ['health']},
                 {'path': '/adm/server/info', 'method': 'get', 'exec': ['jwt', 'info']},
                 {'path': '/adm/logger', 'method': 'get', 'exec': ['jwt', 'getLogger']}]
        chain_file(source, chain, paths, ['health', 'jwt', 'info', 'getLogger'])
        chain = ['health', 'jwt', 'info', 'getLogger', 'proxy']
    p.write(source, 'values.yml', values)
    p.write(source, 'lambda-proxy.yml', {'enabled': True, 'region': 'us-east-1',
            'endpointOverride': 'http://127.0.0.1:19091', 'functions': functions, 'metricsInjection': False})
    return chain


def keys(instance):
    secret = b'local-rie-synthetic-key-not-for-any-real-use-0000000000000000'
    l2.http(instance.fake_url + '/_configure', {'jwks': {'keys': [
        {'kty': 'oct', 'alg': 'HS256', 'kid': 'local-rie-test', 'k': p.b64(secret)}]}})


def rows(instance):
    result = []
    for line in instance.logs().splitlines():
        try:
            result.append(json.loads(line))
        except ValueError:
            pass
    return result


def cors(instance):
    allowed = {'origin': ['https://test.invalid']}
    preflight = dict(allowed, **{'access-control-request-method': ['DELETE']})
    records = [p.request(instance, p.event(headers=allowed), 200),
               p.request(instance, p.event(method='OPTIONS', headers=preflight), 200),
               p.request(instance, p.event(headers={'origin': ['https://denied.invalid']}), 403, 'ERR10092'),
               p.request(instance, p.event(), 200),
               p.request(instance, p.event('/backend-error', headers=allowed), 502, 'ERR14014'),
               p.request(instance, p.event(headers=allowed), 200)]
    for index in (0, 1, 4, 5):
        assert records[index]['response']['multiValueHeaders']['access-control-allow-origin'] == ['https://test.invalid']
    assert records[1]['response']['body'] is None
    assert records[1]['response']['multiValueHeaders']['access-control-allow-methods'] == ['GET,POST']
    for index in (2, 3):
        assert 'access-control-allow-origin' not in records[index]['response']['multiValueHeaders']
    # Native preflight preserves a response but still dispatches once.
    assert [c['method'] for c in instance.state()['invoke']] == ['GET', 'OPTIONS', 'GET', 'GET', 'GET']
    return records


def factors(instance, variant):
    header = 'authorization' if variant == 'basic' else 'x-test-key'
    good = 'Basic ' + base64.b64encode(b'synthetic-user:synthetic-password').decode() if variant == 'basic' else 'synthetic-api-key'
    invalid = 'Basic synthetic-user:wrong' if variant == 'basic' else 'wrong'
    code = 'ERR10047' if variant == 'basic' else 'ERR10075'
    records = [p.request(instance, p.event('/allowed', headers={header: [good]}), 200),
               p.request(instance, p.event('/allowed', headers={header: [invalid]}), 401, code),
               p.request(instance, p.event('/allowed'), 401),
               p.request(instance, p.event('/allowed', headers={header: [good]}), 200)]
    if variant == 'basic':
        records.append(p.request(instance, p.event('/forbidden', headers={header: [good]}), 401, 'ERR10071'))
    else:
        records.append(p.request(instance, p.event('/unprotected'), 200))
    assert len(instance.state()['invoke']) == (2 if variant == 'basic' else 3)
    assert all(c['principalId'] is None for c in instance.state()['invoke'])
    for secret in (good, 'synthetic-password', 'synthetic-api-key'):
        assert secret not in instance.logs(), 'credential leaked into log'
    return records


def limit(instance):
    records = []
    for address, status in [('198.51.100.7', 200), ('198.51.100.7', 429),
                            ('198.51.100.8', 200), ('198.51.100.7', 429), ('198.51.100.8', 429)]:
        record = p.request(instance, p.event(headers={'x-forwarded-for': [address]}), status)
        if status == 429:
            assert json.loads(record['response']['body'])['code'] == 'ERR10088'
            assert record['response']['multiValueHeaders']['ratelimit-limit'] == ['1/HOURS']
            assert record['response']['multiValueHeaders']['ratelimit-remaining'] == ['0']
            reset = record['response']['multiValueHeaders']['ratelimit-reset'][0]
            assert reset.endswith('s') and 0 < int(reset[:-1]) <= 3600
        else:
            assert 'ratelimit-limit' not in record['response']['multiValueHeaders']
        records.append(record)
    assert len(instance.state()['invoke']) == 2
    return records


def telemetry(instance):
    records = []
    for path, status, header in [('/pets/one', 200, {'x-test-mask': ['id=PRIVATE-MASK-A'], 'x-traceability-id': ['trace-A']}),
                                 ('/backend-error', 502, {'x-test-mask': ['id=PRIVATE-MASK-B']}),
                                 ('/unmatched', 400, {}), ('/pets/two', 200, {}),
                                 ('/pets/three', 200, {'x-traceability-id': ['trace-B']})]:
        records.append(p.request(instance, p.event(path, headers=header), status))
    deadline = time.monotonic() + 3
    while True:
        logs = rows(instance)
        audit = [json.loads(r['fields']['audit']) for r in logs if isinstance(r.get('fields', {}).get('audit'), str)]
        emf = [r for r in logs if '_aws' in r]
        if len(audit) == len(emf) == 5:
            break
        assert time.monotonic() < deadline, {'audit': audit, 'emf': emf}
        time.sleep(.02)
    for index, row in enumerate(audit):
        assert row['serviceId'] == 'trusted-l2-service' and row['timestamp'] == 'synthetic-time', row
        assert row['statusCode'] == records[index]['response']['statusCode'], row
        if index < 2:
            assert row['x-test-mask'].startswith('id=***'), row
        else:
            assert 'x-test-mask' not in row, row
    for index, row in enumerate(emf):
        assert set(row) == {'Duration', 'Errors', 'FunctionName', 'Requests', 'Route', '_aws'}, row
        assert row['Route'] == ['/pets/{id}', '/backend-error', '<unmatched>', '/pets/{id}', '/pets/{id}'][index], row
        assert row['Errors'] == int(index in (1, 2)) and row['Requests'] == 1, row
        assert row['FunctionName'] == 'light-lambda-l2' and row['Duration'] >= 0, row
        assert row['_aws']['CloudWatchMetrics'][0]['Namespace'] == 'LightLambda', row
    assert 'PRIVATE-MASK-' not in instance.logs()
    assert len(instance.state()['invoke']) == 4
    # Forwarded trace header is evidence of the wire input only. The formatter
    # suppresses spans, so this does not qualify Traceability's private state.
    assert instance.state()['invoke'][0]['headers']['x-traceability-id'] == 'trace-A'
    assert 'x-traceability-id' not in instance.state()['invoke'][2]['headers']
    return records


def token(instance):
    records = []
    headers = {'service_id': ['fixed-service']}
    for mode, status, code in [('denied', 401, 'ERR10000'), ('malformed', 500, 'ERR14004'), ('valid', 200, None)]:
        l2.http(instance.fake_url + '/_configure', {'tokenMode': mode})
        records.append(p.request(instance, p.event('/x', headers=headers), status, code))
    records.append(p.request(instance, p.event('/x', headers=dict(headers, authorization=['Bearer synthetic-subject'])), 200))
    records.append(p.request(instance, p.event('/other'), 200))
    records.append(p.request(instance, p.event('/x', headers=headers), 200))
    state = instance.state()
    assert len(state['invoke']) == 4 and len(state['oauth']) == 3, state
    assert all(c['clientCredentialsGrant'] and c['clientAuthPresent'] for c in state['oauth']), state
    extra = state['extra']
    assert extra[0]['outboundAccessToken'] and not extra[0]['secondaryAccessToken'], extra
    assert extra[1]['subjectPreserved'] and extra[1]['secondaryAccessToken'], extra
    assert not extra[2]['authorizationPresent'] and not extra[2]['secondaryAccessToken'], extra
    assert extra[3]['outboundAccessToken'] and not extra[3]['secondaryAccessToken'], extra
    for secret in ('synthetic-oauth-secret', 'SYNTHETIC-L2-ACCESS-NOT-A-REAL-TOKEN', 'synthetic-subject'):
        assert secret not in instance.logs()
    return records


def specification(instance, source):
    keys(instance)
    credential = p.token('synthetic-spec-user', scope='read')
    headers = {'authorization': ['Bearer ' + credential]}
    records = [p.request(instance, p.event('/api/write', headers=headers), 403)]
    original = (source / 'openapi.yml').read_bytes()
    (source / 'openapi.yml').write_text('changed-sensitive-marker: [')
    records.extend([p.request(instance, p.event('/api/read', headers=headers), 200),
                    p.request(instance, p.event('/api/write', headers=headers), 403),
                    p.request(instance, p.event('/api/read', headers={'authorization': ['Bearer invalid']}), 401),
                    p.request(instance, p.event('/api/unmatched', headers=headers), 404),
                    p.request(instance, p.event('/api/read', 'POST', headers), 405),
                    p.request(instance, p.event('/api/read', headers=headers), 200)])
    (source / 'openapi.yml').write_bytes(original)
    state = instance.state()
    assert len(state['invoke']) == 2 and state['jwksFetches'] == 1, state
    assert all(c['principalId'] == 'synthetic-spec-user' for c in state['invoke']), state
    assert 'changed-sensitive-marker' not in instance.logs() and credential not in instance.logs()
    return records


def sanitizer(instance):
    encoded = p.event('/x', 'POST', body='eyJ2YWx1ZSI6IjxiPiJ9')
    encoded['isBase64Encoded'] = True
    records = [p.request(instance, encoded, 200),
               p.request(instance, p.event('/x', 'POST', body='{bad'), 400, 'ERR10015'),
               p.request(instance, p.event('/x', 'POST', body='plain <b>'), 200),
               p.request(instance, encoded, 200)]
    calls = instance.state()['invoke']
    assert [c['body'] for c in calls] == ['eyJ2YWx1ZSI6IiZsdDtiJmd0OyJ9', 'plain <b>', 'eyJ2YWx1ZSI6IiZsdDtiJmd0OyJ9'], calls
    assert [c['base64'] for c in instance.state()['extra']] == [True, False, True]
    return records


def admin(instance):
    keys(instance)
    credential = p.token('synthetic-admin')
    auth = {'authorization': ['Bearer ' + credential]}
    records = [p.request(instance, p.event('/health'), 200, body='OK'),
               p.request(instance, p.event('/adm/server/info'), 401),
               p.request(instance, p.event('/adm/server/info', headers=auth), 200,
                         body='{"runtime":"rust","modules":["serverless-core","serverless-aws"]}'),
               p.request(instance, p.event('/adm/logger', headers=auth), 200, body='{"filter":"info"}'),
               p.request(instance, p.event('/adm/logger'), 401),
               p.request(instance, p.event('/health', 'POST'), 404),
               p.request(instance, p.event('/adm/logger', 'POST', auth), 404),
               p.request(instance, p.event('/adm/server/info', headers=auth), 200,
                         body='{"runtime":"rust","modules":["serverless-core","serverless-aws"]}'),
               p.request(instance, p.event('/health'), 200, body='OK'),
               p.request(instance, p.event('/adm/logger', headers=auth), 200, body='{"filter":"info"}')]
    assert instance.state()['invoke'] == [] and instance.state()['jwksFetches'] == 1
    assert credential not in instance.logs()
    assert records[0]['response']['multiValueHeaders']['content-type'] == ['text/plain; charset=utf-8']
    return records


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--package', required=True, type=Path)
    parser.add_argument('--out', required=True, type=Path)
    parser.add_argument('--profile', required=True, choices=FAMILIES)
    args = parser.parse_args()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    os.environ['LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS'] = '1'
    p.write(out, 'package-check.json', l2.pack.check_zip(args.package))
    task = out / 'var-task'
    task.mkdir()
    with zipfile.ZipFile(args.package) as archive:
        (task / 'bootstrap').write_bytes(archive.read('bootstrap'))
    (task / 'bootstrap').chmod(0o755)
    results = []
    for variant in (('basic', 'apikey') if args.profile == 'factors' else (args.profile,)):
        root = out / variant
        root.mkdir()
        source = root / 'source'
        enabled = prepare(source, args.profile, variant)
        report = l2.pack.layer(source, root / 'config-layer.zip', task / 'bootstrap')
        p.write(root, 'layer-report.json', report)
        manifest = l2.pack.extract_layer(root / 'config-layer.zip', root / 'opt')
        instance = l2.Rie(task, root / 'opt', root / 'runtime')
        instance.fake_source = Path(__file__).with_name('remaining_dependencies.py')
        # The fake extends the existing handler; mount its import alongside it.
        # Rie's mount is a single script, so build a self-contained fake source.
        instance.fake_source = root / 'owned-dependencies.py'
        base = Path(__file__).with_name('profile_dependencies.py').read_text()
        extension = Path(__file__).with_name('remaining_dependencies.py').read_text()
        prelude = "import types,sys\nbase=types.ModuleType('profile_dependencies')\n"
        bundled = prelude + 'exec(' + repr(base) + ",base.__dict__)\nsys.modules['profile_dependencies']=base\n" + extension
        instance.fake_source.write_text(bundled)
        with instance:
            if args.profile == 'factors':
                observed = factors(instance, variant)
            elif args.profile == 'specification':
                observed = specification(instance, root / 'opt')
            else:
                observed = {'cors': cors, 'limit': limit, 'telemetry': telemetry, 'token': token,
                            'sanitizer': sanitizer, 'admin': admin}[args.profile](instance)
            state = instance.state()
            assert all(c['signed'] and c['logType'] == 'None' for c in state['invoke']), state
            assert len({c['requestId'] for c in state['invoke']}) == len(state['invoke']), state
            assert state['jwksFetches'] == (1 if args.profile in ('admin', 'specification') else 0), state
            logs = instance.logs()
            assert logs.count('immutable snapshot ready') == 1 and manifest['configDigest'] in logs
            audits = [r['fields'] for r in rows(instance) if r.get('target') == 'audit' and 'finalizations' in r.get('fields', {})]
            assert len(audits) == len(observed) and all(a['finalizations'] == 1 for a in audits), audits
            assert [a['cold_start'] for a in audits] == [True] + [False] * (len(observed) - 1), audits
            results.append({'variant': variant, 'enabledHandlers': enabled, 'qualifiedAliases': QUALIFIED[variant],
                            'serviceRequests': len(observed), 'coldRequests': 1, 'warmRequests': len(observed) - 1,
                            'admissionOnly': False, 'observations': observed,
                            'invokeCalls': len(state['invoke']), 'oauthFetches': len(state['oauth']),
                            'jwksFetches': state['jwksFetches'], 'pass': True})
        for entry in manifest['files']:
            assert l2.pack.digest((root / 'opt' / entry['path']).read_bytes()) == entry['sha256']
    result = {'profile': args.profile, 'level': 'L2 local RIE only', 'cases': results, 'allPassed': True,
              'blockers': {'traceability': 'Selected in telemetry, but private state is not observable: packaged formatter suppresses span fields. Header forwarding alone is not handler proof.'} if args.profile == 'telemetry' else {},
              'excluded': ['Validator', 'Router rewriting', 'precision child', 'paused experiments', 'AWS']}
    p.write(out, 'results.json', result)
    print(json.dumps({'profile': args.profile, 'out': str(out), 'serviceRequests': sum(c['serviceRequests'] for c in results),
                      'qualifiedAliases': [a for c in results for a in c['qualifiedAliases']]}, indent=2))


if __name__ == '__main__':
    main()
