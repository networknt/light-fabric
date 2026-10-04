"""Deployment component checks: subprocesses have bounded, fake adapters only."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import yaml

from test_w7_rollout import ASSETS, HERE, fixture
from w7_rollout import Refusal, docker_instance, migration_needed, validate_plan

WORKSPACE = HERE.parents[3]
CONFIG = WORKSPACE / 'portal-config-loc'
INSTALL = WORKSPACE / 'light-portal-install'


def executable(path, text):
    path.write_text(text)
    path.chmod(0o755)


def marker(root):
    state = root / '.runtime/w7'
    state.mkdir(parents=True)
    (state / 'prepared.json').write_text('{}\n')
    paths = [root / 'w7-assets.json', root / 'bundle/bundle.sha256', state / 'prepared.json']
    (state / 'startup-ready.sha256').write_text(''.join(
        hashlib.sha256(p.read_bytes()).hexdigest() + '  ' + str(p.relative_to(root)) + '\n' for p in paths))


class DeploymentTests(unittest.TestCase):
    def test_owner_access_setup_check_repeat_and_symlink_refusal(self):
        spec = importlib.util.spec_from_file_location('w7_access', HERE / 'w7-readiness-access.py')
        access = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(access)
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            state = root / '.runtime/w7'
            state.mkdir(parents=True, mode=0o700)
            for name in ['prepared.json', 'startup-ready.sha256', 'w7.lock', 'controller-page-authorization']:
                (state / name).write_text('fixture-only\n')
                (state / name).chmod(0o600)
            (state / 'operation.json').write_text('private journal\n')
            (state / 'operation.json').chmod(0o600)
            uid, gid = os.geteuid() + 1, os.getegid()
            with patch.object(access.grp, 'getgrgid', return_value=SimpleNamespace(gr_name='fixture_private_w7')):
                with self.assertRaises(ValueError): access.arrange(root, uid, gid, check=True)
                access.arrange(root, uid, gid)
                access.arrange(root, uid, gid, check=True)
                with self.assertRaises(ValueError): access.arrange(root, os.geteuid(), gid)
                import fcntl
                with (state / 'w7.lock').open('a') as lock:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    with self.assertRaises(BlockingIOError): access.arrange(root, uid, gid)
                self.assertEqual(state.stat().st_mode & 0o777, 0o710)
                self.assertEqual((state / 'w7.lock').stat().st_mode & 0o777, 0o660)
                self.assertEqual((state / 'controller-page-authorization').stat().st_mode & 0o777, 0o640)
                self.assertEqual((state / 'operation.json').stat().st_mode & 0o777, 0o600)
                (state / 'prepared.json').chmod(0o600)
                with self.assertRaises(ValueError): access.arrange(root, uid, gid, check=True)
                access.arrange(root, uid, gid)
                (state / 'controller-page-authorization').unlink()
                (state / 'controller-page-authorization').symlink_to(state / 'operation.json')
                with self.assertRaises(ValueError): access.arrange(root, uid, gid)

    def test_unsafe_deploy_commands_refuse_before_any_adapter_or_side_effect(self):
        for arguments in [['lt', 'restart'], ['lt'], [],
                          ['lt', 'rust', 'restart'], ['lt', 'rust'],
                          ['lt', ''], ['lt', 'rust', '']]:
            with self.subTest(arguments=arguments), tempfile.TemporaryDirectory() as folder:
                fake = Path(folder)
                trace = fake / 'adapter-trace'
                for name in ['docker', 'podman', 'psql', 'curl', 'python3', 'cat', 'mkdir', 'sleep', 'dirname']:
                    executable(fake / name, '#!/bin/bash\necho ADAPTER_CALLED >>"$MOCK_TRACE"\nexit 97\n')
                result = subprocess.run(['/bin/bash', str(CONFIG / 'scripts/deploy-local.sh'), *arguments],
                                        env={'PATH': str(fake), 'MOCK_TRACE': str(trace)},
                                        capture_output=True, text=True, timeout=5)
                self.assertEqual(result.returncode, 2)
                self.assertIn('W7_DEPLOY_REFUSED', result.stderr)
                self.assertIn('lt stage-controller', result.stderr)
                self.assertFalse(trace.exists())

    def test_pure_ledger_repeat_wrong_digest_and_missing_baseline(self):
        self.assertFalse(migration_needed('accepted', 'accepted', 'operational', '0024_workflow_expression_profile'))
        self.assertTrue(migration_needed('', 'accepted', 'operational', '0025_workflow_operation_receipts'))
        self.assertFalse(migration_needed('accepted', 'accepted', 'portal', 'patch_20261002_01_workflow_expression_profile'))
        with self.assertRaisesRegex(Refusal, 'LEDGER_DIGEST_MISMATCH'):
            migration_needed('wrong', 'accepted', 'operational', '0024_workflow_expression_profile')
        with self.assertRaisesRegex(Refusal, 'BASELINE_LEDGER_REQUIRED'):
            migration_needed('', 'accepted', 'operational', '0001_workflow')

    def test_docker_identity_parser_refuses_ambiguous_states(self):
        for state, paused, restarting, valid in [('running', False, False, True),
                                                 ('exited', False, False, True),
                                                 ('paused', True, False, False),
                                                 ('running', True, False, False),
                                                 ('restarting', False, True, False),
                                                 ('dead', False, False, False)]:
            raw = ' '.join(json.dumps(x) for x in ['fixture-id', 'sha256:' + 'a' * 64,
                                                  state, '2026-10-03T16:00:00Z', 'fixture', paused, restarting])
            if valid:
                self.assertEqual(docker_instance(raw)['state'], 'running' if state == 'running' else 'stopped')
            else:
                with self.assertRaises(Refusal): docker_instance(raw)

    def test_main_compose_inventory_and_controller_barrier(self):
        for repo, relative in [(CONFIG, 'all-in-lt'), (INSTALL, '.')]:
            with self.subTest(repo=repo.name):
                stack = repo / relative
                compose = yaml.safe_load((stack / 'docker-compose.yml').read_text())
                root = stack / 'postgres-db/operations'
                plan = json.loads((root / 'w7-plan.template.json').read_text())
                self.assertEqual(set(compose['services']), set(plan['placements'][0]['services']))
                self.assertTrue(all('profiles' not in s for s in compose['services'].values()))
                dependencies = compose['services']['light-workflow']['depends_on']
                self.assertEqual(dependencies['w7-controller-page-readiness']['condition'], 'service_completed_successfully')
                self.assertEqual(dependencies['controller']['condition'], 'service_healthy')
                readiness = compose['services']['w7-controller-page-readiness']
                self.assertIn('W7_READINESS_UID:?', readiness['user'])
                self.assertIn('W7_READINESS_GID:?', readiness['user'])
                self.assertEqual(readiness['image'], compose['services']['controller']['image'])
                for gate in plan['gates']:
                    self.assertIn('catalog_fingerprint', gate)
                    self.assertIn('rollback_catalog_fingerprint', gate)
                for spec in plan['placements'][0]['services'].values():
                    if spec['role'] != 'workflow':
                        self.assertNotIn('supported_profiles', spec)
                for name in ['w7_rollout.py', 'rollout-prepare', 'rollout-activate',
                             'rollback-workflow-expression', 'check-w7-bundle.py',
                             'w7-startup-guard.sh', 'w7-controller-page-check.sh', 'w7-readiness-access.py']:
                    self.assertEqual((root / 'bin' / name).read_bytes(), (HERE / name).read_bytes())

    def test_deployment_resolves_own_checkout(self):
        source = (CONFIG / 'scripts/deploy-local.sh').read_text()
        self.assertNotIn('$BASE_DIR/portal-config-loc/', source)
        self.assertIn('$REPO_DIR/all-in-lt', source)
        self.assertIn('restart-check', source)
        self.assertIn('stage-controller', source)

    def test_invalid_target_and_freshness_plans(self):
        for mutation in [lambda p: p.update(freshness_seconds=91),
                         lambda p: p['gates'][1].update(kind='unclassified'),
                         lambda p: p['gates'][1].update(pgservice=p['gates'][0]['pgservice']),
                         lambda p: p['placements'][0]['services']['workflow'].update(gate_id='portal')]:
            plan, _ = fixture()
            mutation(plan)
            with self.assertRaises(Refusal):
                validate_plan(plan)

    def test_mock_bootstrap_ledger_guard_before_any_side_effect(self):
        """Stop at the first mkdir after all ledger checks: no DB adapter executes."""
        for mode in ['matching', 'missing', 'wrong', 'rolled-back']:
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as folder:
                root = Path(folder) / 'operations'
                shutil.copytree(ASSETS, root)
                (root / 'bin').mkdir(exist_ok=True)
                shutil.copyfile(HERE / 'w7-startup-guard.sh', root / 'bin/w7-startup-guard.sh')
                marker(root)
                manifest = root / 'databases.tsv'
                manifest.write_text(''.join(f'{db}\tfixture.local\tfixture\tfixture\tfixture\n'
                                            for db in ['operations', 'operations_networknt', 'operations_taiji']))
                fake = Path(folder) / 'fake'
                fake.mkdir()
                trace = Path(folder) / 'trace'
                executable(fake / 'psql', '''#!/usr/bin/env python3
import json, os, pathlib, re, sys
args = ' '.join(sys.argv[1:])
with open(os.environ['MOCK_TRACE'], 'a') as f: f.write('query\\n')
if 'migration_digest' in args:
    migration = re.search(r"migration_id='([^']+)'", args).group(1)
    rows = json.loads(pathlib.Path(os.environ['MOCK_ROWS']).read_text())
    value = rows[migration]
    if migration == '0025_workflow_operation_receipts':
        if os.environ['MOCK_MODE'] == 'missing': value = ''
        if os.environ['MOCK_MODE'] == 'wrong': value = 'sha256:wrong'
    print(value)
elif 'to_regprocedure' in args:
    print('f' if os.environ['MOCK_MODE'] == 'rolled-back' else 't')
else:
    sys.exit(88)
''')
                executable(fake / 'mkdir', '#!/usr/bin/env bash\necho MOCK_STARTUP_BOUNDARY\nexit 77\n')
                rows = {e['migrationId']: 'sha256:' + e['sha256'] for e in
                        json.loads((root / 'bundle/manifest.json').read_text())['orderedMigrations']}
                rowfile = Path(folder) / 'rows.json'
                rowfile.write_text(json.dumps(rows))
                env = {'PATH': str(fake) + ':' + os.environ['PATH'],
                       'OPERATIONAL_BUNDLE_ROOT': str(root / 'bundle'),
                       'OPERATIONAL_DATABASE_MANIFEST': str(manifest),
                       'OPERATIONAL_HOST_SECRET_ROOT': str(root / 'private'),
                       'MOCK_TRACE': str(trace), 'MOCK_ROWS': str(rowfile), 'MOCK_MODE': mode}
                command = ['bash', str(CONFIG / 'all-in-lt/postgres-db/operations/bin/bootstrap-operational-databases.sh')]
                for _ in range(2):
                    result = subprocess.run(command, env=env, capture_output=True, text=True, timeout=20)
                    if mode == 'matching':
                        self.assertEqual(result.returncode, 77, result.stderr)
                        self.assertIn('MOCK_STARTUP_BOUNDARY', result.stdout)
                    else:
                        self.assertNotEqual(result.returncode, 0)
                        self.assertNotIn('MOCK_STARTUP_BOUNDARY', result.stdout)
                    self.assertFalse((root / 'private').exists())
                    self.assertNotIn('postgres://', result.stdout + result.stderr)
                if mode == 'matching':
                    self.assertEqual(len(trace.read_text().splitlines()), 2 * 3 * 45)

    def test_paged_probe_fake_curl_only(self):
        for status, body, passed in [('200', '{"items":[],"nextCursor":null}', True),
                                     ('404', '{}', False), ('200', '{"results":[]}', False)]:
            with self.subTest(status=status, body=body), tempfile.TemporaryDirectory() as folder:
                root = Path(folder) / 'operations'
                shutil.copytree(ASSETS, root)
                (root / 'bin').mkdir(exist_ok=True)
                shutil.copyfile(HERE / 'w7-startup-guard.sh', root / 'bin/w7-startup-guard.sh')
                marker(root)
                fake = Path(folder) / 'fake'
                fake.mkdir()
                executable(fake / 'curl', '''#!/usr/bin/env python3
import os, pathlib, sys
args=sys.argv[1:]
assert args[-1] == 'https://controller:8438/internal/execution/results/page?limit=1'
pathlib.Path(args[args.index('--output')+1]).write_text(os.environ['MOCK_BODY'])
print(os.environ['MOCK_STATUS'],end='')
''')
                header = Path(folder) / 'header'
                header.write_text('Authorization: Bearer FAKE_SECRET_SENTINEL\n')
                env = {'PATH': str(fake) + ':' + os.environ['PATH'], 'W7_OPERATIONS_ROOT': str(root),
                       'W7_CONTROLLER_AUTH_HEADER_FILE': str(header), 'MOCK_BODY': body, 'MOCK_STATUS': status}
                result = subprocess.run(['bash', str(HERE / 'w7-controller-page-check.sh')],
                                        env=env, capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode == 0, passed)
                self.assertNotIn('FAKE_SECRET_SENTINEL', result.stdout + result.stderr)


class ControllerLayoutTests(unittest.TestCase):
    def test_staged_validation_missing_and_escaping_dependencies(self):
        spec = importlib.util.spec_from_file_location('stage_context', WORKSPACE / 'controller-rs-e04-w6/scripts/stage-build-context.py')
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            controller, fabric = root / 'source-controller', root / 'source-fabric'
            (controller / 'docker').mkdir(parents=True)
            fabric.mkdir()
            original = '[package]\nname="fixture"\nversion="0.1.0"\n[dependencies]\ndep={path = "../light-fabric-e04/dep"}\n'
            (controller / 'Cargo.toml').write_text(original)
            (controller / 'docker/Dockerfile').write_text('COPY light-fabric/ /usr/src/light-fabric/\n')
            (fabric / 'Cargo.toml').write_text('[workspace]\nmembers=["dep"]\n')
            (fabric / 'dep').mkdir()
            (fabric / 'dep/Cargo.toml').write_text('[package]\nname="dep"\nversion="0.1.0"\n')
            staged = module.stage(controller, fabric, root / 'staged')
            self.assertEqual((controller / 'Cargo.toml').read_text(), original)
            self.assertEqual((staged / 'controller-rs/Cargo.toml').read_text(), original.replace('../light-fabric-e04/', '../light-fabric/'))
            (staged / 'light-fabric/dep/Cargo.toml').unlink()
            with self.assertRaises(ValueError): module.validate(staged)
            (staged / 'light-fabric/dep/Cargo.toml').write_text('[dependencies]\nescape={path="../../../outside"}\n')
            with self.assertRaises(ValueError): module.validate(staged)


if __name__ == '__main__':
    unittest.main()
