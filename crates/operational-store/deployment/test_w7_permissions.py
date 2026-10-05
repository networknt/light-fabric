"""Bounded POSIX identity regression. Requires root only to set fixture UIDs.

No accounts/groups are created; changes are confined to a temporary fixture.
No production adapter, Docker, database or network program is invoked.
"""
import grp
import hashlib
import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
ASSETS = HERE.parent / 'release'
OWNER, CONTAINER, PRIVATE_GROUP, OUTSIDER = 20001, 20002, 20003, 20004
spec = importlib.util.spec_from_file_location('readiness_access', HERE / 'w7-readiness-access.py')
access = importlib.util.module_from_spec(spec)
spec.loader.exec_module(access)


def as_identity(uid, gid, groups, action):
    reader, writer = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(reader)
        try:
            os.setgroups(groups)
            os.setgid(gid)
            os.setuid(uid)
            action()
            os.write(writer, b'OK')
            os._exit(0)
        except BaseException as error:
            os.write(writer, str(type(error).__name__).encode())
            os._exit(1)
    os.close(writer)
    message = os.read(reader, 4096)
    os.close(reader)
    _, status = os.waitpid(pid, 0)
    if status != 0 or message != b'OK':
        raise AssertionError('Fixture identity failed: ' + message.decode())


class DistinctIdentityTests(unittest.TestCase):
    def setUp(self):
        self.assertEqual(os.geteuid(), 0, 'Run only the reviewed isolated UID fixture with root authority')
        self.temp = tempfile.TemporaryDirectory(prefix='w7-permissions-')
        self.addCleanup(self.temp.cleanup)
        self.folder = Path(self.temp.name)
        self.folder.chmod(0o755)
        self.root = self.folder / 'operations'
        shutil.copytree(ASSETS, self.root)
        (self.root / 'bin').mkdir(exist_ok=True)
        for name in ['w7-startup-guard.sh', 'w7-controller-page-check.sh']:
            shutil.copyfile(HERE / name, self.root / 'bin' / name)
        self.state = self.root / '.runtime/w7'
        self.state.mkdir(parents=True, mode=0o700)
        for name, content in [('prepared.json', '{}\n'), ('w7.lock', ''),
                              ('controller-page-authorization', 'Authorization: Bearer FAKE_PRIVATE_SENTINEL\n'),
                              ('operation.json', 'owner-only journal\n')]:
            (self.state / name).write_text(content)
            (self.state / name).chmod(0o600)
        paths = [self.root / 'w7-assets.json', self.root / 'bundle/bundle.sha256', self.state / 'prepared.json']
        (self.state / 'startup-ready.sha256').write_text(''.join(
            hashlib.sha256(p.read_bytes()).hexdigest() + '  ' + str(p.relative_to(self.root)) + '\n' for p in paths))
        (self.state / 'startup-ready.sha256').chmod(0o600)
        fake = self.folder / 'fake'
        fake.mkdir()
        (fake / 'curl').write_text('''#!/usr/bin/env python3
import pathlib,sys
args=sys.argv[1:]
assert args[-1]=='https://controller:8438/internal/execution/results/page?limit=1'
pathlib.Path(args[args.index('--output')+1]).write_text('{"items":[],"nextCursor":null}')
print('200',end='')
''')
        (fake / 'curl').chmod(0o755)
        self.env = {'PATH': str(fake) + ':/usr/bin:/bin', 'W7_OPERATIONS_ROOT': str(self.root),
                    'W7_CONTROLLER_AUTH_HEADER_FILE': str(self.state / 'controller-page-authorization')}
        for p in [self.folder, *self.folder.rglob('*')]:
            os.chown(p, OWNER, OWNER)

    def arrange(self, check=False):
        def action():
            with patch.object(grp, 'getgrgid', return_value=SimpleNamespace(gr_name='fixture_private_w7')):
                access.arrange(self.root, CONTAINER, PRIVATE_GROUP, check)
        as_identity(OWNER, OWNER, [PRIVATE_GROUP], action)

    def test_distinct_uid_marker_lock_header_and_private_journal(self):
        def refused_before():
            result = subprocess.run(['/bin/bash', str(self.root / 'bin/w7-startup-guard.sh'), str(self.root)],
                                    env=self.env, capture_output=True, timeout=10)
            assert result.returncode == 2
        as_identity(CONTAINER, PRIVATE_GROUP, [PRIVATE_GROUP], refused_before)
        self.arrange()
        self.arrange(check=True)
        def ready():
            assert os.geteuid() == CONTAINER and self.state.stat().st_uid == OWNER
            # Actual shell guard opens the shared lock for writing and checks
            # both prepared/marker and accepted bundle checksums.
            result = subprocess.run(['/bin/bash', str(self.root / 'bin/w7-controller-page-check.sh')],
                                    env=self.env, capture_output=True, timeout=15)
            assert result.returncode == 0
            assert b'W7_CONTROLLER_PAGED_API_READY' in result.stdout
            assert b'FAKE_PRIVATE_SENTINEL' not in result.stdout + result.stderr
            assert (self.state / 'controller-page-authorization').read_text().startswith('Authorization: Bearer ')
            for name in ['operation.json']:
                try:
                    (self.state / name).read_text()
                except PermissionError:
                    pass
                else:
                    raise AssertionError('journal leaked')
            for path in [self.state / 'controller-page-authorization', self.state / 'prepared.json', self.state / 'new-file']:
                try:
                    path.open('w')
                except PermissionError:
                    pass
                else:
                    raise AssertionError('readiness obtained write authority')
        as_identity(CONTAINER, PRIVATE_GROUP, [PRIVATE_GROUP], ready)
        def outsider():
            for name in ['prepared.json', 'controller-page-authorization', 'w7.lock']:
                try:
                    (self.state / name).open()
                except PermissionError:
                    pass
                else:
                    raise AssertionError('unrelated principal obtained access')
        as_identity(OUTSIDER, OUTSIDER, [], outsider)

    def test_repeated_prepare_requires_access_refresh_and_busy_lock_refuses(self):
        self.arrange()
        def replace_prepared():
            path = self.state / 'replacement'
            path.write_bytes((self.state / 'prepared.json').read_bytes())
            path.chmod(0o600)
            os.replace(path, self.state / 'prepared.json')
        as_identity(OWNER, OWNER, [PRIVATE_GROUP], replace_prepared)
        with self.assertRaises(AssertionError): self.arrange(check=True)
        self.arrange()
        self.arrange(check=True)
        import fcntl
        with (self.state / 'w7.lock').open('a') as stream:
            fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with self.assertRaises(AssertionError): self.arrange()


if __name__ == '__main__':
    unittest.main()
