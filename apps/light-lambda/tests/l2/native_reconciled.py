#!/usr/bin/env python3
"""Materialize explicit native-sample candidates; never disable Validator."""
import argparse
import json
from pathlib import Path
import shutil
import zipfile
import yaml
import composed as c

NATIVE = Path('/home/steve/workspace/light-lambda-native/src/main/resources/config')
SAMPLE = c.l2.ROOT / 'apps/light-lambda/migrations/native-petstore'


def prepare_candidates(out):
    out.mkdir(parents=True, exist_ok=False)
    native_handler = yaml.safe_load((NATIVE / 'handler.yml').read_text())
    native_values = yaml.safe_load((NATIVE / 'values.yml').read_text())
    full = out / 'reconciled-full'
    c.prepare(full)
    handler = {k: native_handler[k] for k in ['enabled', 'chains', 'paths', 'defaultHandlers']}
    handler['handlers'] = list(native_values['handler.handlers'])
    for alias, suffix in [('requestCors', 'middleware.cors.RequestCorsMiddleware'),
                          ('responseCors', 'middleware.cors.ResponseCorsMiddleware'),
                          ('cache', 'cache.CacheExplorerHandler')]:
        handler['handlers'].append('com.networknt.aws.lambda.handler.' + suffix + '@' + alias)
    c.p.write(full, 'handler.yml', handler)
    shutil.copyfile(SAMPLE / 'openapi.yaml', full / 'openapi.yml')
    proxy = json.loads((full / 'lambda-proxy.yml').read_text())
    # Keep native endpoint keys, including the missing PUT binding. Replace only
    # the cloud function identity with an owned fake, never the request prefix.
    proxy['functions'] = {key: 'test-only-business' for key in native_values['lambda-proxy.functions']}
    c.p.write(full, 'lambda-proxy.yml', proxy)
    c.p.write(full, 'openapi-validator.yml', {'enabled': True, 'skipBodyValidation': False,
                                             'handleNullableField': True, 'skipPathPrefixes': []})
    c.p.write(out, 'native-identities.json', [c.l2.pack.file_id(NATIVE / n) for n in
        ['handler.yml', 'values.yml', 'openapi.yaml']])
    qualification = out / 'validator-qualification'
    qualification.mkdir()
    for name in ['handler.yml', 'values.yml', 'openapi-handler.yml', 'openapi-validator.yml']:
        shutil.copyfile(SAMPLE / name, qualification / name)
    shutil.copyfile(SAMPLE / 'openapi.yaml', qualification / 'openapi.yaml')
    return full, qualification


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    full, qualification = prepare_candidates(args.out.resolve())
    # No layer is advertised as runnable: offline Init is deliberately closed.
    import os
    os.environ['LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS'] = '1'
    bootstrap = c.PACKAGE.with_name('bootstrap')
    assert c.l2.pack.file_id(c.PACKAGE)['sha256'] == '24955f85bcc632989f6c8ed772c611dbab5530b8813abc14026beb64a6a47bc4'
    with zipfile.ZipFile(c.PACKAGE) as archive:
        assert archive.read('bootstrap') == bootstrap.read_bytes(), 'offline probe must use delivered ZIP bytes'
    reports = []
    for source in [full, qualification]:
        try:
            c.l2.pack.validate(bootstrap, source)
        except ValueError as error:
            reports.append({'profile': source.name, 'admitted': False,
                            'coverage': 'admission-only', 'diagnostic': str(error)})
        else:
            raise AssertionError('Validator must remain admission-closed in delivered bootstrap')
    c.p.write(args.out, 'admission-results.json', reports)
    print(json.dumps(reports, indent=2))


if __name__ == '__main__':
    main()
