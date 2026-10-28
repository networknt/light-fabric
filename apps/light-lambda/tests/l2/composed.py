#!/usr/bin/env python3
"""Native-order supported composition through the unchanged packaged bootstrap."""
import argparse
import json
import os
from pathlib import Path
import time
import zipfile
import remaining_profiles as r

p = r.p
l2 = r.l2
PACKAGE = Path('/home/steve/workspace/artifacts/implementation/light-lambda/WP14-WP16-baseline-20261019T115626Z/reproducible-package-v2/bootstrap.zip')
REQUEST = ['metrics', 'limit', 'correlation', 'requestCors', 'requestHeader', 'requestTransformer',
           'audit', 'token', 'router', 'specification', 'security', 'sanitizer']
RESPONSE = ['responseHeader', 'responseCors', 'responseTransformer']
ADMIN = ['correlation', 'specification', 'security']
FIXTURES = ['t/T-01-metadata-native-cel-multi-contract-v1.json',
            't/T-02-response-header-remove-update-multi-contract-v1.json']


def prepare(source):
    l2.source(source)
    aliases = REQUEST + ['proxy'] + RESPONSE + ['health', 'info', 'getLogger']
    p.write(source, 'handler.yml', {'enabled': True,
        'handlers': [r.CLASSES[a] + '@' + a for a in aliases],
        'chains': {'request': REQUEST, 'response': RESPONSE, 'admin': ADMIN},
        'defaultHandlers': ['request', 'proxy', 'response'],
        'paths': [{'path': '/health', 'method': 'get', 'exec': ['health']},
                  {'path': '/adm/server/info', 'method': 'get', 'exec': ['admin', 'info']},
                  {'path': '/adm/logger', 'method': 'get', 'exec': ['admin', 'getLogger']}]})
    values = {'light-lambda.stage': 'local-composed', 'security.enabled': True,
        'unified-security.enabled': True, 'unified-security.pathPrefixAuths': [{'prefix': '/', 'jwt': True}],
        'security.issuer': 'test-issuer', 'security.audience': ['test-audience'],
        'security.skipVerifyScopeWithoutSpec': False, 'client.tokenKeyServerUrl': 'http://127.0.0.1:19091',
        'client.tokenKeyUri': '/jwks', 'client.tokenKeyEnableHttp2': False,
        'client.tokenServerUrl': 'http://127.0.0.1:19091', 'client.tokenCcUri': '/token',
        'client.tokenCcClientId': 'synthetic-client', 'client.tokenCcClientSecret': 'synthetic-oauth-secret',
        'client.tokenEnableHttp2': False, 'client.tokenRenewBeforeExpired': 0,
        'client.expiredRefreshRetryDelay': 0, 'client.earlyRefreshRetryDelay': 0, 'client.maxRequestRetry': 0,
        'cache.caches': [{'cacheName': 'token', 'maxSize': 100, 'expiryInMinutes': 15}],
        'sanitizer.enabled': True, 'sanitizer.bodyEncoder': 'html', 'sanitizer.headerEnabled': False,
        'health.enabled': True, 'info.enableServerInfo': True, 'logger.enabled': True}
    p.write(source, 'values.yml', values)
    p.write(source, 'metrics.yml', {'enabled': True})
    p.write(source, 'limit.yml', {'enabled': True, 'key': 'address', 'rateLimit': '100/h',
        'address': {'198.51.100.7': '100/h', '198.51.100.200': '1/h'}})
    p.write(source, 'correlation.yml', {'enabled': True, 'autogenCorrelationID': True})
    p.write(source, 'cors.yml', {'enabled': True, 'allowedOrigins': ['https://test.invalid'], 'allowedMethods': ['GET', 'POST']})
    p.write(source, 'header.yml', {'enabled': True,
        'request': {'update': {'x-request': 'configured', 'x-remove': 'request-added', 'x-added': 'request-before'}},
        'response': {'remove': ['x-business'], 'update': {'x-added': 'response-before', 'x-remove': 'response-before',
                    'x-chain': 'composed', 'access-control-allow-origin': 'https://headers.invalid'}}})
    for name in ['request-transformer.yml', 'response-transformer.yml']:
        p.write(source, name, {'enabled': True, 'appliedPathPrefixes': ['/x']})
    rule = {'endpointRules': {}, 'ruleBodies': {}}
    identities = []
    for name in FIXTURES:
        path = l2.ROOT / 'tests/serverless-conformance/fixtures/v1' / name
        fixture = json.loads(path.read_text())
        candidate = fixture['ruleConfigurationPair']['rust']['rule']
        assert not (rule['endpointRules'].keys() & candidate['endpointRules'].keys())
        assert not (rule['ruleBodies'].keys() & candidate['ruleBodies'].keys())
        rule['endpointRules'].update(candidate['endpointRules'])
        rule['ruleBodies'].update(candidate['ruleBodies'])
        identities.append({'id': fixture['id'], **l2.pack.file_id(path)})
    p.write(source, 'rule.yml', rule)
    p.write(source, 'audit.yml', {'enabled': True, 'auditOnError': False, 'mask': True,
        'statusCode': True, 'responseTime': False, 'headers': ['x-test-mask'],
        'audit': ['serviceId'], 'timestampFormat': "'synthetic-time'"})
    p.write(source, 'mask.yml', {'regex': {'requestHeader': {'x-test-mask': 'id=(.+)'}}})
    p.write(source, 'lambda-app.yml', {'lambdaAppId': 'trusted-composed-service'})
    p.write(source, 'token.yml', {'enabled': True, 'appliedPathPrefixes': ['/']})
    p.write(source, 'router.yml', {'enabled': True, 'httpsEnabled': False, 'urlRewriteRules': []})
    p.write(source, 'direct-registry.yml', {'directUrls': {'http-service': 'http://127.0.0.1:19091/base'}})
    p.write(source, 'openapi-handler.yml', {'enabled': True})
    operation = lambda scope: {'security': [{'oauth': [scope]}]}
    paths = {'/x': {'get': operation('read'), 'post': operation('read')},
             '/changed': {'post': operation('write')}, '/backend-error': {'get': operation('read')},
             '/disconnect': {'get': operation('read')}, '/redirect': {'get': operation('read')},
             '/adm/server/info': {'get': operation('read')}, '/adm/logger': {'get': operation('read')}}
    p.write(source, 'openapi.yml', {'openapi': '3.1.0', 'components': {'securitySchemes': {'oauth': {'type': 'oauth2'}}}, 'paths': paths})
    p.write(source, 'lambda-proxy.yml', {'enabled': True, 'region': 'us-east-1',
        'endpointOverride': 'http://127.0.0.1:19091', 'metricsInjection': False, 'logType': 'None',
        'functions': {f'{path}@{method}': 'test-only-business' for path, methods in paths.items()
                      if not path.startswith('/adm/') for method in methods}})
    return identities


def fake_script(output):
    # Reuse both established fake transports; add only redacted observations.
    sources = [('profile_dependencies', 'profile_dependencies.py'), ('remaining_dependencies', 'remaining_dependencies.py')]
    bundled = 'import types,sys\n'
    for module, file in sources:
        text = Path(__file__).with_name(file).read_text()
        bundled += f'module=types.ModuleType({module!r})\nexec({text!r},module.__dict__)\nsys.modules[{module!r}]=module\n'
    bundled += Path(__file__).with_name('composed_dependencies.py').read_text()
    output.write_text(bundled)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--package', type=Path, default=PACKAGE)
    parser.add_argument('--package-sha256', default='24955f85bcc632989f6c8ed772c611dbab5530b8813abc14026beb64a6a47bc4',
                        help='explicit qualified ordinary-package identity; defaults to the original delivery')
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    os.environ['LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS'] = '1'
    assert l2.pack.file_id(args.package)['sha256'] == args.package_sha256, 'ordinary package must match the explicitly pinned identity, not instrumentation'
    checked = l2.pack.check_zip(args.package)
    p.write(out, 'package-check.json', checked)
    task = out / 'var-task'
    task.mkdir()
    with zipfile.ZipFile(args.package) as archive:
        (task / 'bootstrap').write_bytes(archive.read('bootstrap'))
    (task / 'bootstrap').chmod(0o755)
    source = out / 'source'
    identities = prepare(source)
    p.write(out, 'fixture-identities.json', identities)
    p.write(out, 'layer-report.json', l2.pack.layer(source, out / 'config-layer.zip', task / 'bootstrap'))
    manifest = l2.pack.extract_layer(out / 'config-layer.zip', out / 'opt')
    instance = l2.Rie(task, out / 'opt', out / 'runtime')
    instance.fake_source = out / 'owned-dependencies.py'
    fake_script(instance.fake_source)
    a = p.token('synthetic-A', scope='read')
    b = p.token('synthetic-B', scope='read')
    writer = p.token('synthetic-writer', scope='write')
    cc = p.token('synthetic-cc', scope='read write')
    observations = []
    audit_cases = []
    metric_cases = []
    business_calls = {}

    def event(path='/x', method='GET', auth=a, origin=True, service=False, body=None, cid=None, address='198.51.100.7'):
        headers = {'x-forwarded-for': [address]}
        if auth is not None:
            headers['authorization'] = ['Bearer ' + auth]
        if origin:
            headers['origin'] = ['https://test.invalid' if origin is True else origin]
        if service:
            headers['service_id'] = ['http-service']
        if cid:
            headers['x-correlation-id'] = [cid]
        return p.event(path, method, headers, body)

    with instance:
        r.keys(instance)
        l2.http(instance.fake_url + '/_configure', {'oauthAccessToken': cc})

        def step(name, raw, status, code=None, sdk=0, http=0, business=True, replacement=False):
            before = instance.state()
            record = p.request(instance, raw, status, code)
            after = instance.state()
            delta = {key: len(after[key]) - len(before[key]) for key in ['invoke', 'http', 'oauth']}
            delta['jwksFetches'] = after['jwksFetches'] - before['jwksFetches']
            assert delta['invoke'] == sdk and delta['http'] == http, (name, delta, record)
            response_headers = record['response']['multiValueHeaders']
            if business:
                if replacement:
                    assert 'x-chain' not in response_headers, (name, record)
                else:
                    assert response_headers['x-chain'] == ['composed'], (name, record)
                metric_cases.append(record)
                # Selected completion descriptors run even when an earlier
                # request policy rejects before AuditMiddleware's request slot.
                audit_cases.append(record)
            if sdk:
                business_calls[name] = after['invoke'][-1]
            observations.append({'name': name, 'observed': record, 'dependencyDelta': delta})
            # Persist incrementally so failed assertions never erase earlier evidence.
            p.write(out, 'observations.json', observations)
            return record, delta

        _, delta = step('cors-before-token-auth', event(auth='invalid', origin='https://denied.invalid'), 403, 'ERR10092')
        assert delta['oauth'] == delta['jwksFetches'] == 0
        l2.http(instance.fake_url + '/_configure', {'tokenMode': 'malformed'})
        step('token-before-router-auth', event(auth='invalid', service=True), 500, 'ERR14004')
        l2.http(instance.fake_url + '/_configure', {'tokenMode': 'valid'})
        good, _ = step('sdk-response-order', event(cid='composed-first'), 200, sdk=1)
        assert good['response']['multiValueHeaders']['x-added'] == ['after']
        assert 'x-remove' not in good['response']['multiValueHeaders'] and 'x-business' not in good['response']['multiValueHeaders']
        assert good['response']['multiValueHeaders']['access-control-allow-origin'] == ['https://test.invalid']
        step('router-selected-before-auth-rejection', event(auth='invalid', service=True), 401, 'ERR10000')
        step('auth-before-sanitizer', event(method='POST', auth='invalid', service=True, body='{bad'), 401, 'ERR10000')
        step('sanitizer-rejection-after-router-selection', event(method='POST', service=True, body='{bad'), 400, 'ERR10015')
        encoded = event(method='POST', body='eyJ2YWx1ZSI6IjxiPiJ9')
        encoded['isBase64Encoded'] = True
        step('sanitizer-before-sdk', encoded, 200, sdk=1)
        assert business_calls['sanitizer-before-sdk']['body'] == 'eyJ2YWx1ZSI6IiZsdDtiJmd0OyJ9'
        step('transform-before-spec-and-security', event(method='POST', body='mutate'), 403)
        step('transformed-http-target', event(method='POST', auth=writer, service=True, body='mutate'), 200, http=1)
        step('transformed-sdk-target', event(method='POST', auth=writer, body='mutate'), 200, sdk=1)
        changed = business_calls['transformed-sdk-target']
        assert changed['path'] == '/changed' and changed['body'] == 'transformed'
        assert changed['headers']['x-added'] == 'updated' and 'x-remove' not in changed['headers']
        assert instance.state()['composedInvoke'][-1]['addedQuery'] == '7'
        step('http-response-order', event(service=True), 200, http=1)
        step('combined-business-error', event('/backend-error'), 502, 'ERR14014', sdk=1)
        step('combined-http-error', event('/disconnect', service=True), 400, 'ERR10089', http=1)
        _, delta = step('transform-error-before-auth', event(method='POST', auth='invalid', body='x' * 32768), 500, 'ERR14032', replacement=True)
        assert delta['oauth'] == delta['jwksFetches'] == 0
        step('cors-rejection-consumes-early-quota', event(auth='invalid', origin='https://denied.invalid', address='198.51.100.200'), 403, 'ERR10092')
        step('limiter-before-cors-and-auth', event(auth='invalid', origin='https://denied.invalid', address='198.51.100.200'), 429, 'ERR10088')
        step('business-primary-oauth-without-caller-token', event(auth=None, service=True), 200, http=1)
        step('business-missing-token-without-service-id', event(auth=None), 401, 'ERR10002')
        warm, _ = step('warm-principal-origin-isolation', event(auth=b, origin=False), 200, sdk=1)
        assert warm['response']['multiValueHeaders']['access-control-allow-origin'] == ['https://headers.invalid']
        assert business_calls['warm-principal-origin-isolation']['principalId'] == 'synthetic-B'
        generated = business_calls['warm-principal-origin-isolation']['headers']['x-correlation-id']
        assert generated and generated != 'composed-first'
        step('warm-path-body-query-isolation', event(method='POST', body='original'), 200, sdk=1)
        fresh = business_calls['warm-path-body-query-isolation']
        assert fresh['path'] == '/x' and fresh['body'] == 'original' and fresh['headers']['x-added'] == 'request-before'
        assert instance.state()['composedInvoke'][-1]['addedQuery'] is None
        step('health-bypasses-business-chain', event('/health', auth=None, origin=False), 200, business=False)
        info, delta = step('shared-admin-chain-info', event('/adm/server/info'), 200, business=False)
        assert info['response']['body'] == '{"runtime":"rust","modules":["serverless-core","serverless-aws"]}'
        assert 'x-chain' not in info['response']['multiValueHeaders'] and delta['oauth'] == 0
        _, delta = step('admin-no-token-without-oauth-handler', event('/adm/server/info', auth=None), 401, 'ERR10002', business=False)
        assert delta['oauth'] == 0
        logger, _ = step('shared-admin-chain-logger', event('/adm/logger'), 200, business=False)
        assert logger['response']['body'] == '{"filter":"info"}'
        step('composed-http-redirect-no-follow', event('/redirect', service=True), 302, http=1)
        step('final-warm-sdk', event(origin=False, cid='composed-last'), 200, sdk=1)
        assert business_calls['final-warm-sdk']['headers']['x-correlation-id'] == 'composed-last'

        state = instance.state()
        assert len(state['invoke']) == 7 and len(state['http']) == 5, state
        assert len(state['oauth']) == 2 and state['jwksFetches'] == 1, state
        # Native LambdaRouter constructs host + request path, without attaching
        # the event query map. SDK forwarding separately preserves added=7.
        assert state['http'][0]['path'] == '/base/changed' and state['http'][0]['body'] == 'transformed', state
        assert state['composedHttp'][0]['added'] == 'updated', state
        assert all(not c['serviceIdPresent'] for c in state['http']), state
        assert [c['secondaryAccessToken'] for c in state['composedHttp']] == [True, True, True, False, True], state
        assert [c['outboundAccessToken'] for c in state['composedHttp']] == [False, False, False, True, False], state
        assert all(c['signed'] and c['logType'] == 'None' for c in state['invoke']), state
        assert [c['principalId'] for c in state['invoke']] == [
            'synthetic-A', 'synthetic-A', 'synthetic-writer', 'synthetic-A',
            'synthetic-B', 'synthetic-A', 'synthetic-A'], state
        assert [c['path'] for c in state['http']] == [
            '/base/changed', '/base/x', '/base/disconnect', '/base/x', '/base/redirect'], state
        assert all(not c['secondaryAccessToken'] for c in state['extra']), state
        assert len({c['requestId'] for c in state['invoke']}) == 7, state
        deadline = time.monotonic() + 3
        while True:
            logs = r.rows(instance)
            audit = [json.loads(x['fields']['audit']) for x in logs if isinstance(x.get('fields', {}).get('audit'), str)]
            metrics = [x for x in logs if '_aws' in x]
            if len(audit) == len(audit_cases) and len(metrics) == len(metric_cases):
                break
            assert time.monotonic() < deadline, {'actualAudit': len(audit), 'expectedAudit': len(audit_cases), 'actualMetrics': len(metrics), 'expectedMetrics': len(metric_cases)}
            time.sleep(.02)
        assert [x['statusCode'] for x in audit] == [x['response']['statusCode'] for x in audit_cases], audit
        assert all(x['serviceId'] == 'trusted-composed-service' for x in audit), audit
        assert [x['Errors'] for x in metrics] == [int(x['response']['statusCode'] >= 400) for x in metric_cases], metrics
        assert all(x['Requests'] == 1 and x['Duration'] >= 0 and x['Route'] in ['/x', '/changed', '/backend-error', '/disconnect', '/redirect'] for x in metrics), metrics
        framework = [x['fields'] for x in logs if x.get('target') == 'audit' and 'finalizations' in x.get('fields', {})]
        assert len(framework) == len(observations) and all(x['finalizations'] == 1 for x in framework), framework
        assert [x['cold_start'] for x in framework] == [True] + [False] * (len(observations) - 1)
        assert instance.logs().count('immutable snapshot ready') == 1
        assert not any(secret in instance.logs() for secret in [a, b, writer, cc, 'synthetic-oauth-secret', 'SYNTHETIC-L2-ACCESS-NOT-A-REAL-TOKEN'])
        p.write(out, 'policy-observations.json', {'audit': audit, 'metrics': metrics, 'framework': framework})
        p.write(out, 'results.json', {'profile': 'native-order-supported-composed-v1', 'allPassed': True,
            'package': checked, 'requestChain': REQUEST, 'responseChain': RESPONSE, 'adminChain': ADMIN,
            'serviceRequests': len(observations), 'coldRequests': 1, 'warmRequests': len(observations) - 1,
            'sdkInvokes': len(state['invoke']), 'httpCalls': len(state['http']), 'oauthFetches': len(state['oauth']),
            'jwksFetches': state['jwksFetches'], 'auditPolicyRecords': len(audit), 'emfRecords': len(metrics),
            'observations': observations, 'admissionOnlyRequests': 0, 'traceabilityInstrumentedEvidenceIncluded': False,
            'excluded': ['Traceability state proof', 'Validator', 'Router rewriting', 'paused precision work', 'AWS', 'full WP12/WP13']})
    for entry in manifest['files']:
        assert l2.pack.digest((out / 'opt' / entry['path']).read_bytes()) == entry['sha256']
    assert all(l2.pack.file_id(Path(x['path']))['sha256'] == x['sha256'] for x in identities)
    print(json.dumps({'out': str(out), 'composedServiceRequestsPassed': len(observations), 'sdkInvokes': 7, 'httpCalls': 5}, indent=2))


if __name__ == '__main__':
    main()
