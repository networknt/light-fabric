#!/usr/bin/env python3
"""JWT-enabled direct trial via pinned bootstrap; owned synthetic JWKS, never real tokens."""
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
from qualify_direct import PACKAGE, SHA


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    assert hashlib.sha256(PACKAGE.read_bytes()).hexdigest() == SHA
    os.environ['LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS'] = '1'
    task = out / 'var-task'
    task.mkdir()
    with zipfile.ZipFile(PACKAGE) as archive:
        (task / 'bootstrap').write_bytes(archive.read('bootstrap'))
    (task / 'bootstrap').chmod(0o755)
    c.p.write(out, 'aws-layer-report.json', c.l2.pack.layer(HERE / 'config', out / 'aws-config-layer.zip', task / 'bootstrap'))
    source = out / 'local-config'
    shutil.copytree(HERE / 'config', source)
    values = json.loads((source / 'values.yml').read_text())
    values.update({'client.tokenKeyServerUrl': 'http://127.0.0.1:19091',
                   'client.tokenKeyUri': '/jwks', 'client.tokenKeyEnableHttp2': False})
    c.p.write(source, 'values.yml', values)
    proxy = json.loads((source / 'lambda-proxy.yml').read_text())
    proxy['endpointOverride'] = 'http://127.0.0.1:19091'
    c.p.write(source, 'lambda-proxy.yml', proxy)
    c.p.write(out, 'local-layer-report.json', c.l2.pack.layer(source, out / 'local-config-layer.zip', task / 'bootstrap'))
    c.l2.pack.extract_layer(out / 'local-config-layer.zip', out / 'opt')
    original = c.l2.docker

    def local_docker(*args, **kwargs):
        substitutions = {'AWS_REGION=us-east-1': 'AWS_REGION=us-east-2',
            'LIGHT_LAMBDA_GATEWAY_ARN=arn:aws:lambda:us-east-1:000000000000:function:light-lambda-l2':
                'LIGHT_LAMBDA_GATEWAY_ARN=arn:aws:lambda:us-east-2:964637446810:function:light-lambda-l2'}
        return original(*(substitutions.get(x, x) if isinstance(x, str) else x for x in args), **kwargs)

    c.l2.docker = local_docker
    instance = c.l2.Rie(task, out / 'opt', out / 'runtime')
    instance.fake_source = out / 'owned-dependencies.py'
    c.fake_script(instance.fake_source)
    observations = []
    tokens = []
    try:
        with instance:
            c.r.keys(instance)

            def step(name, event_name, scope=None, status=200, code=None, dispatch=1, **claims):
                event = json.loads((HERE / 'events' / (event_name + '.json')).read_text())
                if scope is not None:
                    token = c.p.token(name, iss=values['security.issuer'], aud=values['security.audience'][0], scope=scope, **claims)
                    tokens.append(token)
                    event['multiValueHeaders']['authorization'] = ['Bearer ' + token]
                before = instance.state()
                observed = c.p.request(instance, event, status, code)
                after = instance.state()
                assert len(after['invoke']) - len(before['invoke']) == dispatch
                assert len(after['http']) == len(after['oauth']) == 0
                if dispatch:
                    call = after['invoke'][-1]
                    assert call['principalId'] == name and call['path'] == event['path']
                    assert call['method'] == event['httpMethod']
                observations.append({'case': name, 'result': observed, 'invokeDelta': dispatch,
                                     'jwksFetchDelta': after['jwksFetches'] - before['jwksFetches']})
                c.p.write(out, 'observations.json', observations)

            step('missing-token', 'list', status=401, code='ERR10002', dispatch=0)
            step('wrong-scope', 'list', scope='unrelated', status=403, dispatch=0)
            step('list-reader', 'list', scope='portal.r')
            # Native matchedScopes and Rust matches_scope both use ANY match.
            step('post-reader-native-any-match', 'post', scope='portal.r')
            step('post-writer', 'post', scope='portal.r portal.w')
            step('get-reader', 'get', scope='portal.r')
            step('delete-reader-denied', 'delete', scope='portal.r', status=403, dispatch=0)
            step('delete-writer', 'delete', scope='portal.w')
            step('malformed-json', 'malformed-json', scope='portal.r portal.w', status=400, code='ERR10015', dispatch=0)
            c.l2.http(instance.fake_url + '/_configure', {'backend': {'statusCode': 502, 'body': 'synthetic backend error', 'isBase64Encoded': False}})
            step('backend-error', 'list', scope='portal.r', status=502)
            c.l2.http(instance.fake_url + '/_configure', {'backend': {'statusCode': 200, 'body': 'recovered', 'isBase64Encoded': False}})
            step('warm-reader', 'list', scope='portal.r')
            state = instance.state()
            assert len(state['invoke']) == 7 and state['jwksFetches'] == 1
            assert state['invoke'][-1]['body'] is None
            logs = instance.logs()
            assert logs.count('immutable snapshot ready') == 1
            assert all(token not in logs for token in tokens)
            c.p.write(out, 'results.json', {'allPassed': True, 'serviceRequests': len(observations),
                'invokes': 7, 'jwksFetches': 1, 'httpCalls': 0, 'oauthCalls': 0,
                'awsCalls': 0, 'realDevJwksCalls': 0, 'realTokensSaved': False,
                'scope': 'Pinned application Init/service/SDK, synthetic HS256 JWKS and tokens; issuer/audience and portal scope policy retained',
                'notProved': ['Live development JWKS TLS fetch', 'Real RS256 token through Rust', 'AWS execution']})
    finally:
        c.l2.docker = original
    print(json.dumps({'out': str(out), 'requestsPassed': len(observations)}))


if __name__ == '__main__':
    main()
