#!/usr/bin/env python3
"""Local, explicitly partial qualification of the owner-selected petstore project."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import sys
import zipfile

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'tests/l2'))
import composed as c
import native_reconciled as native

SOURCE = Path('/home/steve/workspace/lambda-petstore')
PACKAGE = Path('/home/steve/workspace/artifacts/implementation/light-lambda/WP13-D08e-release-20261021T155728Z/release-package/bootstrap.zip')
SHA = 'd095e89c07f6079823d6d07395b8716f280add5e7e09f7a75b1d03e121af756a'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    assert hashlib.sha256(PACKAGE.read_bytes()).hexdigest() == SHA
    os.environ['LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS'] = '1'
    c.p.write(out, 'package-check.json', c.l2.pack.check_zip(PACKAGE))
    task = out / 'var-task'
    task.mkdir()
    with zipfile.ZipFile(PACKAGE) as archive:
        (task / 'bootstrap').write_bytes(archive.read('bootstrap'))
    (task / 'bootstrap').chmod(0o755)
    values = yaml.safe_load((SOURCE / 'config/values.yml').read_text())
    inherited = yaml.safe_load((native.NATIVE / 'handler.yml').read_text())
    c.p.write(out, 'source-inventory.json', {
        'source': str(SOURCE), 'configurationFiles': [c.l2.pack.file_id(p) for p in sorted((SOURCE / 'config').iterdir()) if p.is_file()],
        'valuesKeys': sorted(values), 'handlerDeclarations': values['handler.handlers'],
        'inheritedHandlerTemplate': c.l2.pack.file_id(native.NATIVE / 'handler.yml'),
        'chains': inherited['chains'], 'paths': inherited['paths'],
        'defaultHandlers': inherited['defaultHandlers'],
        'functionRouteKeys': sorted(values['lambda-proxy.functions']),
        'sourceSecurity': {k: values[k] for k in ['security.enableVerifyJwt', 'unified-security.enabled']},
        'sourceLimiter': values['limit.rateLimit'],
        'template': c.l2.pack.file_id(SOURCE / 'template.yaml')})
    full, validator = native.prepare_candidates(out / 'full-candidates')
    for source in [full, validator]:
        shutil.copyfile(SOURCE / 'config/openapi.yaml', source / ('openapi.yml' if source == full else 'openapi.yaml'))
    h = json.loads((full / 'handler.yml').read_text())
    h['handlers'] = list(values['handler.handlers']) + [
        'com.networknt.aws.lambda.handler.middleware.cors.RequestCorsMiddleware@requestCors',
        'com.networknt.aws.lambda.handler.middleware.cors.ResponseCorsMiddleware@responseCors',
        'com.networknt.aws.lambda.handler.cache.CacheExplorerHandler@cache']
    c.p.write(full, 'handler.yml', h)
    admissions = []
    for source in [full, validator]:
        try:
            c.l2.pack.validate(task / 'bootstrap', source)
        except ValueError as error:
            admissions.append({'profile': source.name, 'coverage': 'admission-only', 'admitted': False, 'diagnostic': str(error)})
        else:
            raise AssertionError('full Validator profile must remain closed')
    c.p.write(out, 'admission-results.json', admissions)
    for auth in [False, True]:
        profile = out / ('auth-variant' if auth else 'source-auth-disabled-subset')
        profile.mkdir()
        source = profile / 'source'
        c.prepare(source)
        shutil.copyfile(SOURCE / 'config/openapi.yaml', source / 'openapi.yml')
        proxy = json.loads((source / 'lambda-proxy.yml').read_text())
        proxy['functions'] = {k: 'test-only-business' for k in values['lambda-proxy.functions']}
        c.p.write(source, 'lambda-proxy.yml', proxy)
        local = json.loads((source / 'values.yml').read_text())
        local['security.enabled'] = auth
        local['unified-security.enabled'] = auth
        c.p.write(source, 'values.yml', local)
        c.p.write(profile, 'layer-report.json', c.l2.pack.layer(source, profile / 'config-layer.zip', task / 'bootstrap'))
        c.l2.pack.extract_layer(profile / 'config-layer.zip', profile / 'opt')
        instance = c.l2.Rie(task, profile / 'opt', profile / 'runtime')
        instance.fake_source = profile / 'owned-dependencies.py'
        c.fake_script(instance.fake_source)
        observations = []
        with instance:
            c.r.keys(instance)

            def step(name, path='/v1/pets', method='GET', body=None, scope='read:pets', token=True, status=200, code=None, dispatch=0):
                headers = {'x-forwarded-for': ['198.51.100.7'], 'x-traceability-id': ['synthetic-petstore'], 'content-type': ['application/json']}
                if auth and token:
                    headers['authorization'] = ['Bearer ' + (c.p.token(name, scope=scope) if token is True else token)]
                before = instance.state()
                result = c.p.request(instance, c.p.event(path, method, headers, body), status, code)
                after = instance.state()
                delta = {k: len(after[k])-len(before[k]) for k in ['invoke', 'http', 'oauth']}
                delta['jwksFetches'] = after['jwksFetches']-before['jwksFetches']
                assert delta['invoke'] == dispatch and delta['http'] == delta['oauth'] == 0, (name, delta)
                observations.append({'name': name, 'observed': result, 'dependencyDelta': delta})
                c.p.write(profile, 'observations.json', observations)
                return after

            if auth:
                step('missing-token', token=False, status=401, code='ERR10002')
                step('invalid-token', token='invalid', status=401, code='ERR10000')
                step('wrong-scope', scope='unrelated', status=403)
            step('list', dispatch=1)
            step('get-pet', path='/v1/pets/17', dispatch=1)
            step('create-pet', method='POST', body='{"id":17,"name":"Milo"}', scope='read:pets write:pets', dispatch=1)
            step('delete-pet', path='/v1/pets/17', method='DELETE', scope='write:pets', dispatch=1)
            step('malformed-json', method='POST', body='{bad', scope='read:pets write:pets', status=400, code='ERR10015')
            c.l2.http(instance.fake_url + '/_configure', {'backend': {'statusCode': 502, 'body': 'synthetic backend error', 'isBase64Encoded': False}})
            step('backend-error', status=502, dispatch=1)
            c.l2.http(instance.fake_url + '/_configure', {'backend': {'statusCode': 200, 'body': 'recovered', 'isBase64Encoded': False}})
            state = step('warm-recovery', dispatch=1)
            assert state['invoke'][-1]['path'] == '/v1/pets' and state['invoke'][-1]['body'] is None
            assert [x['principalId'] for x in state['invoke']] == (['list', 'get-pet', 'create-pet', 'delete-pet', 'backend-error', 'warm-recovery'] if auth else [None]*6)
            assert state['jwksFetches'] == (1 if auth else 0)
            c.p.write(profile, 'results.json', {'allPassed': True, 'fullConfigurationCompatible': False,
                'coverage': 'service-execution-supported-subset', 'authVariant': auth,
                'requests': len(observations), 'invokes': len(state['invoke']), 'jwksFetches': state['jwksFetches'],
                'schemaValidationProved': False, 'observations': observations})
    print(json.dumps({'out': str(out), 'fullConfigurationCompatible': False, 'subsetRequestsPassed': 17}))


if __name__ == '__main__':
    main()
