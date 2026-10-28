"""Hermetic checks for active-checkout and sibling template synchronization."""
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


HEADER = (
    '# Generated from light-fabric/crates/light-client/config/client.yml.\n'
    '# Update via light-fabric/scripts/sync-client-config.py; '
    'see its manifest for fallback overrides.\n'
)
CANONICAL = 'request:\n  timeout: ${client.timeout:3000}\n'
RENDERED = HEADER + 'request:\n  timeout: ${client.timeout:4000}\n'
SUFFIX = 'apps/light-lambda/config/client.yml'


class SyncClientConfig(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.workspace = Path(temp.name)
        self.fabric = self.workspace / 'feature-worktree'
        (self.fabric / 'scripts').mkdir(parents=True)
        shutil.copyfile(Path(__file__).with_name('sync-client-config.py'),
                        self.fabric / 'scripts/sync-client-config.py')
        self.write(self.fabric / 'crates/light-client/config/client.yml', CANONICAL)
        self.write(self.fabric / SUFFIX, RENDERED)
        self.targets = {'light-fabric/' + SUFFIX: {'client.timeout': '4000'}}
        self.manifest()

    def write(self, path, text):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def manifest(self):
        self.write(self.fabric / 'scripts/client-config-targets.json', json.dumps(self.targets))

    def run_sync(self, check=True):
        return subprocess.run(
            [sys.executable, str(self.fabric / 'scripts/sync-client-config.py'),
             '--workspace', str(self.workspace), *(['--check'] if check else [])],
            capture_output=True, text=True, timeout=10)

    def test_noncanonical_checkout_is_checked_without_sibling(self):
        result = self.run_sync()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('Checked 1 templates', result.stdout)

    def test_shadow_checkout_does_not_replace_active_checkout(self):
        self.write(self.workspace / 'light-fabric' / SUFFIX, 'shadow sentinel\n')
        self.assertEqual(self.run_sync().returncode, 0)

    def test_active_drift_is_rejected_even_when_shadow_matches(self):
        self.write(self.workspace / 'light-fabric' / SUFFIX, RENDERED)
        self.write(self.fabric / SUFFIX, 'drift\n')
        result = self.run_sync()
        self.assertEqual(result.returncode, 1)
        self.assertIn('Drift: light-fabric/' + SUFFIX, result.stdout)

    def test_unregistered_active_template_is_rejected(self):
        self.write(self.fabric / 'apps/unregistered/config/client.yml', RENDERED)
        result = self.run_sync()
        self.assertEqual(result.returncode, 1)
        self.assertIn('Unregistered template: light-fabric/apps/unregistered/config/client.yml',
                      result.stderr)

    def test_missing_active_template_is_not_skipped(self):
        (self.fabric / SUFFIX).unlink()
        result = self.run_sync()
        self.assertEqual(result.returncode, 1)
        self.assertIn('Missing registered template:', result.stderr)

    def test_explicit_workspace_still_checks_siblings(self):
        suffix = 'apps/portal-service/config/client.yml'
        self.targets['portal-service/' + suffix] = {}
        self.manifest()
        self.write(self.workspace / 'portal-service' / suffix, HEADER + CANONICAL)
        self.assertEqual(self.run_sync().returncode, 0)
        self.write(self.workspace / 'portal-service' / suffix, 'drift\n')
        self.assertEqual(self.run_sync().returncode, 1)

    def test_sync_writes_active_checkout_and_preserves_shadow(self):
        self.write(self.fabric / SUFFIX, 'drift\n')
        shadow = self.workspace / 'light-fabric' / SUFFIX
        self.write(shadow, 'shadow sentinel\n')
        self.assertEqual(self.run_sync(check=False).returncode, 0)
        self.assertEqual((self.fabric / SUFFIX).read_text(), RENDERED)
        self.assertEqual(shadow.read_text(), 'shadow sentinel\n')


if __name__ == '__main__':
    unittest.main()
