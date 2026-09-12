#!/usr/bin/env python3
"""Pinned, credential-free Codex configuration precedence qualification."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import queue
import re
import subprocess
import tempfile
import threading


class Server:
    def __init__(self, binary, home, cwd, overrides=()):
        self.process = subprocess.Popen([str(binary), *overrides, 'app-server'], cwd=cwd,
            env={**os.environ, 'CODEX_HOME': str(home)}, stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        self.frames = queue.Queue()
        def reader():
            for line in self.process.stdout:
                self.frames.put(json.loads(line))
            self.frames.put(None)
        threading.Thread(target=reader, daemon=True).start()
        self.next_id = 0
        self.call('initialize', {'clientInfo': {'name': 'light-config-qualification', 'version': '1'}})

    def call(self, method, params, expected_error=None):
        self.next_id += 1
        self.process.stdin.write(json.dumps(dict(id=self.next_id, method=method, params=params)) + '\n')
        self.process.stdin.flush()
        while True:
            value = self.frames.get(timeout=20)
            assert value is not None, 'App Server exited'
            if value.get('id') == self.next_id:
                if expected_error is not None:
                    assert expected_error in value.get('error', {}).get('message', ''), 'Expected native configuration rejection'
                    return value['error']
                assert 'error' not in value, 'App Server rejected qualification request'
                return value['result']

    def close(self):
        self.process.kill()
        self.process.wait(timeout=10)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--codex', type=Path, required=True)
    args = parser.parse_args()
    source = (Path(__file__).resolve().parents[1] / 'crates/coding-agent-runtime/src/lib.rs').read_text()
    expected = re.search(r'CODEX_APP_SERVER_BINARY_DIGEST: &str\s*=\s*"sha256:([a-f0-9]+)"', source)[1]
    assert hashlib.sha256(args.codex.read_bytes()).hexdigest() == expected, 'unqualified native binary'
    with tempfile.TemporaryDirectory(prefix='codex-personal-config-') as directory:
        root = Path(directory)
        home, repo = root / 'home', root / 'repository'
        home.mkdir(); repo.mkdir()
        subprocess.run(['git', 'init', '-q', str(repo)], check=True)
        config = home / 'config.toml'
        for name in ('rules', 'skills', 'plugins'):
            (home / name).mkdir()
        for name in ('AGENTS.md', 'managed_config.toml'):
            (home / name).touch()
        (home / 'instructions.md').write_text('Qualification instructions.\n')
        config.write_text('model="test-native-default"\napproval_policy="on-request"\nsandbox_mode="read-only"\nmodel_instructions_file="instructions.md"\n')
        server = Server(args.codex, home, repo)
        try:
            initial = server.call('config/read', {'cwd': str(repo), 'includeLayers': True})
            assert initial['config']['model_instructions_file'] == str(home / 'instructions.md')
            thread = server.call('thread/start', {'cwd': str(repo), 'ephemeral': True})
            assert thread['model'] == 'test-native-default'
            assert thread['approvalPolicy'] == 'on-request' and thread['sandbox']['type'] == 'readOnly'
            trusted = server.call('thread/start', {'cwd': str(repo), 'ephemeral': True,
                'approvalPolicy': 'never', 'sandbox': 'danger-full-access', 'model': 'test-explicit'})
            assert trusted['model'] == 'test-explicit'
            assert trusted['approvalPolicy'] == 'never' and trusted['sandbox']['type'] == 'dangerFullAccess'
        finally:
            server.close()
        # The pinned Config response omits model_providers. Reserved-provider
        # protection comes from the native loader, not an absent response field.
        config.write_text('[model_providers.openai]\nname="override-probe"\nbase_url="https://example.invalid/v1"\nwire_api="responses"\n')
        server = Server(args.codex, home, repo)
        try:
            server.call('config/read', {'cwd': str(repo), 'includeLayers': True},
                expected_error='reserved built-in provider IDs')
            server.call('thread/start', {'cwd': str(repo), 'ephemeral': True},
                expected_error='reserved built-in provider IDs')
        finally:
            server.close()
        # Config discovery uses the staged repository, not ignored files in a source checkout.
        home = root / 'project-home'; home.mkdir()
        config = home / 'config.toml'
        config.write_text('model="test-native-default"\n')
        dot = repo / '.codex'; dot.mkdir()
        (dot / 'config.toml').write_text('model="test-project-model"\n')
        server = Server(args.codex, home, repo)
        try:
            layers = server.call('config/read', {'cwd': str(repo), 'includeLayers': True})['layers']
            assert any(l['name']['type'] == 'project' and l.get('disabledReason') for l in layers)
        finally:
            server.close()
        config.write_text(config.read_text() + '\n[projects.' + json.dumps(str(repo)) + ']\ntrust_level="trusted"\n')
        server = Server(args.codex, home, repo)
        try:
            current = server.call('config/read', {'cwd': str(repo), 'includeLayers': True})
            assert current['config']['model'] == 'test-project-model'
            assert [l['version'] for l in current['layers']] != [l['version'] for l in initial['layers']]
        finally:
            server.close()
        # Requirements are native managed policy, mounted only in this test namespace.
        requirements = root / 'requirements.toml'
        requirements.write_text('allowed_approval_policies=["never"]\nallowed_sandbox_modes=["read-only"]\n')
        config.write_text('model="test-native-default"\napproval_policy="never"\nsandbox_mode="read-only"\n')
        arguments = ['--ro-bind', '/', '/', '--bind', str(root), str(root), '--tmpfs', '/etc',
            '--dir', '/etc/codex', '--ro-bind', str(requirements), '/etc/codex/requirements.toml', '--', str(args.codex)]
        server = Server(Path('/usr/bin/bwrap'), home, repo, arguments)
        try:
            required = server.call('configRequirements/read', {})['requirements']
            assert required['allowedSandboxModes'] == ['read-only']
            try:
                server.call('thread/start', {'cwd': str(repo), 'ephemeral': True,
                    'approvalPolicy': 'never', 'sandbox': 'danger-full-access'})
            except AssertionError:
                pass
            else:
                raise AssertionError('trusted automation bypassed managed requirements')
        finally:
            server.close()
        print('PASS: pinned inheritance, explicit overrides, managed restrictions, project trust, and reload revisions')


if __name__ == '__main__':
    main()
