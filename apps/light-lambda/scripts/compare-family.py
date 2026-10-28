#!/usr/bin/env python3
"""Select a registered-handler adapter batch; execute the existing Rust runner/comparator."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import time

repo = Path(__file__).resolve().parents[3]
selection = json.loads((repo / 'tests/serverless-conformance/metadata/wp13-family-adapters-v1.json').read_text())['families']
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('family', choices=sorted(selection))
parser.add_argument('--java', required=True, type=Path)
parser.add_argument('--out', required=True, type=Path)
args = parser.parse_args()
out = args.out.resolve()
out.mkdir()  # Every invocation preserves prior results and requires a fresh directory.
java = args.java.resolve(strict=True)
env = os.environ.copy()
# TokenPolicyProvider requires an explicit process flag for owned HTTP loopback.
# It still validates the destination; no TLS policy or production setting changes.
env['LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS'] = '1'
env['AWS_EC2_METADATA_DISABLED'] = 'true'


def execute(name, argv):
    start = time.monotonic()
    with (out / (name + '.log')).open('x') as log:
        result = subprocess.run(argv, cwd=repo, env=env, stdout=log, stderr=subprocess.STDOUT)
    with (out / (name + '.json')).open('x') as target:
        json.dump(dict(argv=argv, exit=result.returncode, elapsedSeconds=time.monotonic()-start), target, indent=2)
    return result.returncode


if execute('build', ['rtk', 'cargo', 'build', '--locked', '--offline', '-p', 'light-lambda', '--bin', 'bootstrap']):
    raise SystemExit('bootstrap build failed; see ' + str(out))
common = ['rtk', 'cargo', 'run', '--locked', '--offline', '-p', 'serverless-conformance', '--']
fixtures = [arg for id in selection[args.family]['fixtures'] for arg in ['--fixture', id]]
run_status = execute('run', common + ['run', '--out', str(out / 'rust')] + fixtures)
compare_status = execute('compare', common + ['compare', '--java', str(java), '--rust', str(out / 'rust'), '--out', str(out / 'comparison')] + fixtures)
for action, path in [('Rust expectations', out / 'rust/coverage.json'), ('Differential', out / 'comparison/coverage.json')]:
    if path.exists():
        coverage = json.loads(path.read_text())
        print(f'{args.family}: {action}: {coverage["pass"]} pass / {coverage["fail"]} fail; '
              f'{coverage["admissionOnlyPasses"]} admission-only / {coverage["serviceExecutionPasses"]} service executions')
print(f'runExit={run_status} compareExit={compare_status}; selected batch only; WP12/WP13 remain incomplete')
raise SystemExit(1 if run_status or compare_status else 0)
