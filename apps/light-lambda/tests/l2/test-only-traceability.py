#!/usr/bin/env python3
"""Focused RIE test for a separately identified instrumented example package."""
import argparse
import json
import os
from pathlib import Path
import zipfile
import profiles as p

l2 = p.l2


def captures(instance):
    found = []
    for line in instance.logs().splitlines():
        try:
            row = json.loads(line)
            if row.get('testOnlyObservation') == 'traceability-state-v1':
                found.append(row)
        except ValueError:
            pass
    return found


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--instrumented-package', required=True, type=Path)
    parser.add_argument('--baseline-bootstrap', required=True, type=Path)
    parser.add_argument('--out', required=True, type=Path)
    args = parser.parse_args()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    os.environ['LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS'] = '1'
    checked = l2.pack.check_zip(args.instrumented_package)
    checked['profile'] = 'test-only-traceability-observer-v1'
    checked['qualification'] = 'instrumented only; ZIP/ELF format check is not baseline qualification'
    p.write(out, 'instrumented-package-check.json', checked)
    task = out / 'var-task'
    task.mkdir()
    with zipfile.ZipFile(args.instrumented_package) as archive:
        (task / 'bootstrap').write_bytes(archive.read('bootstrap'))
    (task / 'bootstrap').chmod(0o755)
    assert l2.pack.file_id(task / 'bootstrap')['sha256'] != l2.pack.file_id(args.baseline_bootstrap)['sha256']
    results = []
    for enabled in (True, False):
        name = 'enabled' if enabled else 'disabled-control'
        root = out / name
        root.mkdir()
        source = l2.source(root / 'source')
        p.write(source, 'handler.yml', {'handlers': [
            'com.networknt.aws.lambda.handler.middleware.traceability.TraceabilityMiddleware@trace',
            'com.networknt.aws.lambda.handler.middleware.proxy.LambdaProxyMiddleware@proxy'],
            'defaultHandlers': ['trace', 'proxy']})
        p.write(source, 'traceability.yml', {'enabled': enabled})
        # Admission is validated by the unchanged baseline; the instrumented
        # executable then performs its own actual online Init against this layer.
        report = l2.pack.layer(source, root / 'config-layer.zip', args.baseline_bootstrap)
        p.write(root, 'layer-report.json', report)
        manifest = l2.pack.extract_layer(root / 'config-layer.zip', root / 'opt')
        instance = l2.Rie(task, root / 'opt', root / 'runtime')
        instance.fake_source = Path(__file__).with_name('profile_dependencies.py')
        observed = []
        expected = []
        with instance:
            cases = [('trace-probe-A', '/hello', 200), (None, '/hello', 200)]
            if enabled:
                cases += [('trace-probe-B', '/backend-error', 502), (None, '/hello', 200),
                          ('Bearer PRIVATE-TRACE-MARKER', '/hello', 200), ('trace-probe-A', '/hello', 200)]
            for value, path, status in cases:
                headers = {'x-traceability-id': [value]} if value is not None else {}
                before = len(captures(instance))
                observed.append(p.request(instance, p.event(path, headers=headers), status,
                                          'ERR14014' if status == 502 else None))
                actual = captures(instance)
                assert len(actual) == before + 1, actual
                expected_value = value if enabled and value in ('trace-probe-A', 'trace-probe-B') else (
                    '<redacted>' if enabled and value is not None else None)
                expected.append({'testOnlyObservation': 'traceability-state-v1',
                                 'statePresent': enabled and value is not None, 'traceabilityId': expected_value})
                assert actual == expected, actual
            ingress = None
            if enabled:
                invalid = p.event(headers={'x-traceability-id': ['trace-probe-A']})
                del invalid['multiValueHeaders']
                ingress = p.request(instance, invalid, 400, 'ERR14012')
                assert captures(instance) == expected, 'ingress rejection must not dispatch or observe state'
            state = instance.state()
            assert len(state['invoke']) == len(cases) and state['jwksFetches'] == 0, state
            assert all(c['signed'] and c['logType'] == 'None' for c in state['invoke']), state
            logs = instance.logs()
            assert logs.count('immutable snapshot ready') == 1 and manifest['configDigest'] in logs
            assert 'PRIVATE-TRACE-MARKER' not in logs
            results.append({'profile': name, 'serviceRequests': len(cases), 'admissionRequests': int(ingress is not None),
                            'observations': observed, 'ingress': ingress, 'actualStateCaptures': captures(instance),
                            'sdkInvokes': len(state['invoke']), 'jwksFetches': state['jwksFetches'], 'pass': True})
        for entry in manifest['files']:
            assert l2.pack.digest((root / 'opt' / entry['path']).read_bytes()) == entry['sha256']
    result = {'profile': 'test-only-traceability-observer-v1', 'instrumentedPackage': checked,
              'baselineBootstrap': l2.pack.file_id(args.baseline_bootstrap), 'allPassed': True,
              'cases': results, 'defaultPackageQualification': 'unchanged: traceability private state still unobservable',
              'excluded': ['Validator', 'Router rewriting', 'precision child', 'paused experiments', 'AWS']}
    p.write(out, 'results.json', result)
    print(json.dumps({'out': str(out), 'instrumentedProfilesPassed': len(results),
                      'serviceRequests': sum(c['serviceRequests'] for c in results),
                      'admissionRequests': sum(c['admissionRequests'] for c in results)}, indent=2))


if __name__ == '__main__':
    main()
