#!/usr/bin/env python3
"""Run the existing 19 local profiles; no build, AWS, or precision child."""
import argparse
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--package', type=Path, required=True)
    parser.add_argument('--sha256', required=True)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    package = args.package.resolve()
    assert hashlib.sha256(package.read_bytes()).hexdigest() == args.sha256
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    here = Path(__file__).resolve().parent
    l2 = here.parent
    root = here.parents[4]
    jobs = [
        ('validator', [here / 'validator.py', '--package', package, '--sha256', args.sha256]),
        ('jwt-composition', [here / 'jwt.py']),
        ('backend', [here / 'expanded.py', '--profile', 'backend']),
        ('jwks', [here / 'expanded.py', '--profile', 'jwks']),
    ]
    for name in ('headers', 'jwt', 'router', 'transformers'):
        jobs.append((name, [l2 / 'profiles.py', '--package', package, '--profile', name]))
    for name in ('cors', 'limit', 'factors', 'telemetry', 'token', 'specification', 'sanitizer', 'admin'):
        jobs.append((name, [l2 / 'remaining_profiles.py', '--package', package, '--profile', name]))
    jobs.extend([
        ('composed', [l2 / 'composed.py', '--package', package, '--package-sha256', args.sha256]),
        ('logger-set', [l2 / 'logger_set.py', '--package', package]),
        ('baseline', [here / 'baseline_selected.py', '--package', package]),
    ])
    env = dict(os.environ, LIGHT_LAMBDA_TEST_PACKAGE=str(package),
               LIGHT_LAMBDA_TEST_PACKAGE_SHA256=args.sha256)
    receipts = []

    def run(job):
        name, selection = job
        assert hashlib.sha256(package.read_bytes()).hexdigest() == args.sha256
        command = [sys.executable, '-B', *map(str, selection), '--out', str(out / name)]
        with (out / (name + '.log')).open('w') as log:
            result = subprocess.run(command, cwd=root, env=env, stdout=log, stderr=subprocess.STDOUT)
        return {'profile': name, 'exitCode': result.returncode, 'command': command}

    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        for future in concurrent.futures.as_completed([pool.submit(run, job) for job in jobs]):
            receipt = future.result()
            receipts.append(receipt)
            print(receipt['profile'], receipt['exitCode'], flush=True)
            (out / 'exits.json').write_text(json.dumps(receipts, indent=2) + '\n')
    assert hashlib.sha256(package.read_bytes()).hexdigest() == args.sha256
    raise SystemExit(any(row['exitCode'] != 0 for row in receipts))


if __name__ == '__main__':
    main()
