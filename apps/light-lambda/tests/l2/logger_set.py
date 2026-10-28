#!/usr/bin/env python3
"""Opt-in partial X-04 through the ordinary release ZIP and existing RIE harness."""
import argparse
import json
import os
from pathlib import Path
import shutil
import zipfile
import remaining_profiles as r

p, l2 = r.p, r.l2


def runtime(task, opt, out, **options):
    instance = l2.Rie(task, opt, out, **options)
    instance.fake_source = Path(__file__).with_name('profile_dependencies.py')
    return instance


def prepare(source):
    l2.source(source)
    example = l2.APP / 'examples/logger-set-qualification'
    for name in ['handler.yml', 'logging.yml']:
        shutil.copyfile(example / name, source / name)
    handler = json.loads((source / 'handler.yml').read_text())
    for route in handler['paths']:
        route['path'] = '/adm/logger'
    p.write(source, 'handler.yml', handler)
    p.write(source, 'values.yml', {'light-lambda.stage': 'local-logger-set',
        'logger.enabled': True, 'security.enabled': True,
        'security.issuer': 'test-issuer', 'security.audience': ['test-audience'],
        'security.skipVerifyScopeWithoutSpec': True,
        'client.tokenKeyServerUrl': 'http://127.0.0.1:19091',
        'client.tokenKeyUri': '/jwks', 'client.tokenKeyEnableHttp2': False})
    p.write(source, 'lambda-proxy.yml', {'enabled': True, 'region': 'us-east-1',
        'endpointOverride': 'http://127.0.0.1:19091',
        'functions': {'/*@get': 'test-only-business', '/*@post': 'test-only-business'},
        'logType': 'None', 'metricsInjection': False})


def qualify(instance):
    r.keys(instance)
    token = p.token('synthetic-logger-admin')
    auth = {'authorization': ['Bearer ' + token]}
    records = []

    def request(method, body=None, headers=auth, status=200):
        value = p.request(instance, p.event('/adm/logger', method,
            dict(headers, **{'content-type': ['application/json']}), body), status)
        records.append(value)
        return value['response']

    def current(level):
        assert json.loads(request('GET')['body']) == {'filter': level}

    def command(level):
        return json.dumps([{'name': 'ROOT', 'level': level}])

    current('info')
    request('POST', command('DEBUG')); current('debug')
    request('POST', command('TRACE'), headers={}, status=401); current('debug')
    request('POST', command('TRACE'), headers={'authorization': ['Bearer invalid-token']}, status=401); current('debug')
    request('POST', command('OFF'), status=400); current('debug')
    request('POST', command('TRACE')); current('trace')
    secret = 'logger-rie-private-sentinel'
    request('POST', json.dumps([{'name': 'ROOT', 'level': 'TRACE', 'secret': secret}]), status=400); current('trace')
    request('POST', '[]'); current('trace')
    request('DELETE', command('INFO'), status=404); current('trace')
    boundary = command('DEBUG')
    boundary += ' ' * (1024 - len(boundary))
    request('POST', boundary); current('debug')
    request('POST', boundary + ' ', status=400); current('debug')
    request('POST', command('INFO')); current('info')
    state = instance.state()
    assert state['invoke'] == [] and state['jwksFetches'] == 1, state
    logs = instance.logs()
    assert token not in logs and secret not in logs
    audits = [row['fields'] for row in r.rows(instance)
              if row.get('target') == 'audit' and 'finalizations' in row.get('fields', {})]
    assert len(audits) == len(records) and all(row['finalizations'] == 1 for row in audits)
    assert [row['cold_start'] for row in audits] == [True] + [False] * (len(records) - 1)
    assert logs.count('immutable snapshot ready') == 1
    return records


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--package', required=True, type=Path)
    parser.add_argument('--out', required=True, type=Path)
    args = parser.parse_args()
    out = args.out.resolve(); out.mkdir(parents=True, exist_ok=False)
    os.environ['LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS'] = '1'
    p.write(out, 'package-check.json', l2.pack.check_zip(args.package))
    task = out / 'var-task'; task.mkdir()
    with zipfile.ZipFile(args.package) as archive:
        (task / 'bootstrap').write_bytes(archive.read('bootstrap'))
    (task / 'bootstrap').chmod(0o755)
    source = out / 'source'; prepare(source)
    before = {file.name: file.read_bytes() for file in source.iterdir()}
    # Ambient permission cannot authorize a layer; only the explicit tooling flag can.
    os.environ['LIGHT_LAMBDA_ALLOW_LOGGER_SET'] = '1'
    try:
        l2.pack.validate(task / 'bootstrap', source)
    except ValueError as error:
        assert 'logger SET unsupported' in str(error), str(error)
    else:
        raise AssertionError('ambient logger gate leaked into default layer tooling')
    report = l2.pack.layer(source, out / 'config-layer.zip', task / 'bootstrap', allow_logger_set=True)
    p.write(out, 'layer-report.json', report)
    manifest = l2.pack.extract_layer(out / 'config-layer.zip', out / 'opt')
    assert before == {file.name: file.read_bytes() for file in source.iterdir()}
    with runtime(task, out / 'opt', out / 'enabled', logger_set=True) as instance:
        observations = qualify(instance)
    # Same immutable layer in a fresh process without the trusted startup gate.
    with runtime(task, out / 'opt', out / 'gate-closed') as instance:
        try:
            l2.invoke(instance, p.event('/adm/logger', 'GET'))
        except (OSError, ValueError):
            pass
        assert 'logger SET unsupported' in instance.logs()
        assert instance.state()['invoke'] == [] and instance.state()['jwksFetches'] == 0
    disabled = out / 'logger-disabled'
    shutil.copytree(out / 'opt', disabled)
    (disabled / 'light-lambda-layer.json').unlink()
    values = json.loads((disabled / 'values.yml').read_text())
    values['logger.enabled'] = False
    p.write(disabled, 'values.yml', values)
    # This fixture is YAML; write the single authoritative field as JSON/YAML.
    p.write(disabled, 'logging.yml', {'enabled': False})
    # Deliberately invalid admission input, not a published configuration layer.
    with runtime(task, disabled, out / 'disabled-runtime', logger_set=True) as instance:
        try:
            l2.invoke(instance, p.event('/adm/logger', 'GET'))
        except (OSError, ValueError):
            pass
        assert 'logger SET unsupported' in instance.logs()
        assert instance.state()['invoke'] == [] and instance.state()['jwksFetches'] == 0
    with runtime(task, out / 'opt', out / 'fresh-process', logger_set=True) as instance:
        r.keys(instance)
        response = p.request(instance, p.event('/adm/logger', headers={
            'authorization': ['Bearer ' + p.token('synthetic-fresh-admin')]}), 200)
        assert json.loads(response['response']['body']) == {'filter': 'info'}
        assert instance.state()['invoke'] == [] and instance.state()['jwksFetches'] == 1
    for entry in manifest['files']:
        assert l2.pack.digest((out / 'opt' / entry['path']).read_bytes()) == entry['sha256']
    result = {'profile': 'partial-X04-opt-in', 'level': 'local RIE only', 'allPassed': True,
        'serviceRequests': len(observations) + 1, 'admissionOnlyCases': 2,
        'observations': observations, 'invokeCalls': 0, 'jwksFetches': 2,
        'freshProcessFilter': 'info', 'originalZipModified': False,
        'excluded': ['arbitrary logger names/levels', 'CacheExplorer', 'Validator',
                     'Router rewriting', 'paused experiments', 'AWS']}
    p.write(out, 'results.json', result)
    print(json.dumps({'out': str(out), 'serviceRequests': result['serviceRequests'], 'allPassed': True}))


if __name__ == '__main__':
    main()
