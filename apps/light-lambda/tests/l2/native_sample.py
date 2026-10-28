#!/usr/bin/env python3
"""Explicit supported-subset qualification of the checked-in native pet sample."""
import argparse
import json
import os
from pathlib import Path
import shutil
import zipfile
import yaml
import composed as c


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    os.environ['LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS'] = '1'
    p, l2 = c.p, c.l2
    native = Path('/home/steve/workspace/light-lambda-native/src/main/resources/config')
    snapshot = out / 'native-public-sample'
    snapshot.mkdir()
    # Only public templates/specification; never copy truststores or credentials.
    names = ['handler.yml', 'lambda-app.yml', 'lambda-proxy.yml', 'openapi.yaml']
    for name in names:
        shutil.copyfile(native / name, snapshot / name)
    values = yaml.safe_load((native / 'values.yml').read_text())
    p.write(out, 'native-source-identities.json', [l2.pack.file_id(native / n) for n in names + ['values.yml']])
    p.write(snapshot, 'values-structural-only.json', {k: values[k] for k in
        ['handler.handlers', 'lambda-proxy.functions', 'unified-security.pathPrefixAuths', 'openapi-validator.validateRequest']})
    p.write(out, 'native-key-inventory.json', sorted(values))
    checked = l2.pack.check_zip(c.PACKAGE)
    assert l2.pack.file_id(c.PACKAGE)['sha256'] == '24955f85bcc632989f6c8ed772c611dbab5530b8813abc14026beb64a6a47bc4'
    task = out / 'var-task'
    task.mkdir()
    with zipfile.ZipFile(c.PACKAGE) as z:
        (task / 'bootstrap').write_bytes(z.read('bootstrap'))
    (task / 'bootstrap').chmod(0o755)
    source = out / 'rust-supported-subset'
    c.prepare(source)
    shutil.copyfile(native / 'openapi.yaml', source / 'openapi.yml')
    spec = yaml.safe_load((source / 'openapi.yml').read_text())
    proxy = json.loads((source / 'lambda-proxy.yml').read_text())
    proxy['functions'] = {f'{path}@{method}': 'test-only-business' for path, methods in spec['paths'].items() for method in methods}
    p.write(source, 'lambda-proxy.yml', proxy)
    # Prove enabled Validator is rejected, independently of the subset run.
    blocked = out / 'validator-admission-probe'
    shutil.copytree(source, blocked)
    handler = json.loads((blocked / 'handler.yml').read_text())
    handler['handlers'].append('com.networknt.aws.lambda.handler.middleware.validator.ValidatorMiddleware@validator')
    handler['chains']['request'].append('validator')
    p.write(blocked, 'handler.yml', handler)
    p.write(blocked, 'openapi-validator.yml', {'enabled': True, 'validateRequest': True})
    try:
        l2.pack.validate(task / 'bootstrap', blocked)
    except ValueError as error:
        p.write(out, 'validator-admission.json', {'rejected': True, 'diagnostic': str(error), 'coverage': 'admission-only'})
    else:
        raise AssertionError('Validator must remain unavailable')
    p.write(out, 'layer-report.json', l2.pack.layer(source, out / 'config-layer.zip', task / 'bootstrap'))
    l2.pack.extract_layer(out / 'config-layer.zip', out / 'opt')
    instance = l2.Rie(task, out / 'opt', out / 'runtime')
    instance.fake_source = out / 'owned-dependencies.py'
    c.fake_script(instance.fake_source)
    observations = []
    with instance:
        c.r.keys(instance)
        def step(name, path='/pets', auth=True, scope='pets.read', method='GET', body=None, status=200, code=None, dispatch=0, origin=True):
            headers = {'x-forwarded-for': ['198.51.100.7'], 'x-traceability-id': ['sample-trace']}
            if auth:
                headers['authorization'] = ['Bearer ' + (c.p.token(name, scope=scope) if auth is True else auth)]
            if origin:
                headers['origin'] = ['https://test.invalid' if origin is True else origin]
            before = instance.state()
            result = p.request(instance, p.event(path, method, headers, body), status, code)
            after = instance.state()
            delta = {k: len(after[k]) - len(before[k]) for k in ['invoke', 'http', 'oauth']}
            delta['jwksFetches'] = after['jwksFetches'] - before['jwksFetches']
            assert delta['invoke'] == dispatch and delta['http'] == delta['oauth'] == 0, (name, delta)
            observations.append({'name': name, 'observed': result, 'dependencyDelta': delta})
            p.write(out, 'observations.json', observations)
            return after
        step('cors-before-auth', auth='invalid', origin='https://denied.invalid', status=403, code='ERR10092')
        step('missing-token', auth=False, status=401, code='ERR10002')
        step('invalid-signature', auth='invalid', status=401, code='ERR10000')
        step('wrong-native-scope', scope='read', status=403)
        step('native-list', dispatch=1)
        step('native-template', path='/pets/17', dispatch=1)
        step('native-write', method='POST', scope='pets.uad', body='{"name":"sample"}', dispatch=1)
        # Deliberately invalid against CreatePetRequest: record the validation gap,
        # rather than pretending Sanitizer's JSON parsing is schema validation.
        step('schema-invalid-dispatch-known-gap', method='POST', scope='pets.uad', body='{}', dispatch=1)
        step('malformed-json', method='POST', scope='pets.uad', body='{bad', status=400, code='ERR10015')
        l2.http(instance.fake_url + '/_configure', {'backend': {'statusCode': 502, 'body': 'synthetic backend error', 'isBase64Encoded': False}})
        step('backend-error', status=502, dispatch=1)
        l2.http(instance.fake_url + '/_configure', {'backend': {'statusCode': 200, 'body': 'recovered', 'isBase64Encoded': False}})
        state = step('warm-recovery', dispatch=1, origin=False)
        assert [x['principalId'] for x in state['invoke']] == ['native-list', 'native-template', 'native-write', 'schema-invalid-dispatch-known-gap', 'backend-error', 'warm-recovery']
        assert state['invoke'][-1]['path'] == '/pets' and state['invoke'][-1]['body'] is None
        assert state['jwksFetches'] == 1
        p.write(out, 'results.json', {'sampleOnly': True, 'fullConfigurationCompatible': False,
            'allPassed': True, 'serviceRequests': len(observations), 'warmRequests': len(observations)-1,
            'invoke': len(state['invoke']), 'http': len(state['http']), 'oauth': len(state['oauth']),
            'jwks': state['jwksFetches'], 'package': checked, 'observations': observations,
            'schemaValidationProved': False, 'traceabilityStateProved': False})
    print(json.dumps({'out': str(out), 'passed': len(observations)}))


if __name__ == '__main__':
    main()
