#!/usr/bin/env python3
"""Direct-Invoke trial configuration through the pinned bootstrap and existing RIE harness."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import sys
import zipfile

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parents[1] / 'tests/l2'))
import composed as c

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
    c.p.write(out, 'aws-layer-report.json', c.l2.pack.layer(HERE / 'config', out / 'aws-config-layer.zip', task / 'bootstrap'))
    local = out / 'local-config'
    shutil.copytree(HERE / 'config', local)
    proxy = json.loads((local / 'lambda-proxy.yml').read_text())
    targets = dict(proxy['functions'])
    proxy['endpointOverride'] = 'http://127.0.0.1:19091'
    c.p.write(local, 'lambda-proxy.yml', proxy)
    c.p.write(out, 'local-layer-report.json', c.l2.pack.layer(local, out / 'local-config-layer.zip', task / 'bootstrap'))
    c.l2.pack.extract_layer(out / 'local-config-layer.zip', out / 'opt')
    # Reuse Rie lifecycle/owned dependency containers; only runtime identity changes.
    original_docker = c.l2.docker

    def trial_docker(*args, **kwargs):
        substitutions = {
            'AWS_REGION=us-east-1': 'AWS_REGION=us-east-2',
            'LIGHT_LAMBDA_GATEWAY_ARN=arn:aws:lambda:us-east-1:000000000000:function:light-lambda-l2':
                'LIGHT_LAMBDA_GATEWAY_ARN=arn:aws:lambda:us-east-2:964637446810:function:light-lambda-l2'}
        return original_docker(*(substitutions.get(x, x) if isinstance(x, str) else x for x in args), **kwargs)

    c.l2.docker = trial_docker
    instance = c.l2.Rie(task, out / 'opt', out / 'runtime')
    bundled = out / 'bundled-dependencies.py'
    c.fake_script(bundled)
    instance.fake_source = out / 'owned-dependencies.py'
    instance.fake_source.write_text(
        'import types,sys\n'
        'module=types.ModuleType("composed_trial")\n'
        f'exec({bundled.read_text()!r},module.__dict__)\n'
        'from http.server import ThreadingHTTPServer\n'
        'import profile_dependencies as base\n'
        'base.state["sdkPaths"]=[]\n'
        'class TrialHandler(module.ComposedDependencies):\n'
        ' def do_POST(self):\n'
        '  if self.path.endswith("/invocations"):\n'
        '   with base.lock: base.state["sdkPaths"].append(self.path)\n'
        '  super().do_POST()\n'
        'ThreadingHTTPServer(("0.0.0.0",19091),TrialHandler).serve_forever()\n')
    observations = []
    expected_targets = []
    try:
        with instance:
            def step(name, status=200, dispatch=1, code=None):
                event = json.loads((HERE / 'events' / (name + '.json')).read_text())
                before = instance.state()
                observed = c.p.request(instance, event, status, code)
                after = instance.state()
                delta = len(after['invoke']) - len(before['invoke'])
                assert delta == dispatch, (name, after)
                assert len(after['http']) == len(after['oauth']) == after['jwksFetches'] == 0
                if dispatch:
                    route = ('/v1/pets/{petId}' if name in ['get', 'delete'] else '/v1/pets') + '@' + event['httpMethod'].lower()
                    expected = targets[route]
                    from urllib.parse import unquote
                    assert unquote(after['sdkPaths'][-1]) == '/2015-03-31/functions/' + expected + '/invocations', after['sdkPaths']
                    call = after['invoke'][-1]
                    assert call['path'] == event['path'] and call['method'] == event['httpMethod']
                    # Production sanitizer serializes valid JSON compactly; keep
                    # the observed bytes and compare the intended JSON content.
                    if event['body'] is not None:
                        assert json.loads(call['body']) == json.loads(event['body'])
                    else:
                        assert call['body'] is None
                    assert call['principalId'] is None
                    assert call['signed'] and call['logType'] == 'None'
                    expected_targets.append(expected)
                observations.append({'case': name, 'response': observed, 'invokeDelta': delta})
                c.p.write(out, 'observations.json', observations)

            for name in ['list', 'post', 'get', 'delete']:
                step(name)
            step('malformed-json', 400, 0, 'ERR10015')
            # A backend HTTP/application error is configurable without new routing.
            c.l2.http(instance.fake_url + '/_configure', {'backend': {'statusCode': 502, 'body': 'synthetic backend error', 'isBase64Encoded': False}})
            step('list', 502)
            c.l2.http(instance.fake_url + '/_configure', {'backend': {'statusCode': 200, 'body': 'recovered', 'isBase64Encoded': False}})
            step('list')
            # Owner's existing console payload is an API Gateway example, not
            # supported ALB ingress. Never persist or replay its bearer token.
            step('native-console-sanitized', 400, 0, 'ERR14010')
            step('list')
            state = instance.state()
            assert len(state['invoke']) == 7
            assert len({x['requestId'] for x in state['invoke']}) == 7
            assert state['invoke'][-1]['body'] is None
            assert instance.logs().count('immutable snapshot ready') == 1
            c.p.write(out, 'results.json', {'allPassed': True, 'totalRequests': len(observations),
                'invokes': len(state['invoke']), 'jwksFetches': state['jwksFetches'],
                'serviceExecutionRequests': len(observations) - 1,
                'ingressAdmissionRequests': 1,
                'fourDistinctTargets': len(set(expected_targets)) == 4, 'awsCalls': 0,
                'scope': 'RIE direct Invoke with generated ALB-shaped payloads; local endpoint override only',
                'notProved': ['Java execution', 'AWS IAM', 'API Gateway ingress', 'ALB infrastructure', 'Validator', 'rewriting']})
    finally:
        c.l2.docker = original_docker
    print(json.dumps({'out': str(out), 'requestsPassed': len(observations)}))


if __name__ == '__main__':
    main()
