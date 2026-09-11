#!/usr/bin/env python3
"""Live Codex workspace smoke in a disposable three-repository task. No GitHub writes."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import queue
import threading
import subprocess
import tempfile
import time
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--implement', action='store_true')
    parser.add_argument('--profile', type=Path, required=True)
    parser.add_argument('--codex', type=Path, required=True)
    parser.add_argument('--codex-home', type=Path, required=True)
    parser.add_argument('--worker', type=Path, default=Path('target/debug/light-agent-worker'))
    parser.add_argument('--manager', type=Path, default=Path('target/debug/light-workspace'))
    args = parser.parse_args()
    profile = json.loads(args.profile.read_text())
    keys = ('schemaVersion adapterId adapterVersion adapterProtocolVersion actionKind compatibilityDigest imageDigest capabilityDigest templateId templateVersion templateDigest executable binaryDigest schemaDigest requiredFeatures').split()
    contract = {key: profile[key] for key in keys}
    worker = args.worker.resolve()
    caps = json.loads(subprocess.check_output([worker, 'print-capabilities']))
    with tempfile.TemporaryDirectory(prefix='workspace-chat-smoke-') as directory:
        root = Path(directory)
        repositories = []
        for name in ['backend', 'frontend', 'docs']:
            repo = root / name
            repo.mkdir()
            subprocess.run(['git', 'init', '-q', '-b', 'develop', repo], check=True)
            (repo / 'README.md').write_text(f'{name}: workspace test marker {name.upper()}-42\n')
            subprocess.run(['git', '-C', repo, 'add', '.'], check=True)
            subprocess.run(['git', '-C', repo, '-c', 'user.name=Smoke', '-c', 'user.email=smoke@example.invalid', 'commit', '-qm', 'fixture'], check=True)
            repositories.append(dict(name=name, source=str(repo), integrationBranch='develop', releaseBranch='master'))
        workspace = dict(schemaVersion=1, id='smoke', hostId='host', agents=['codex'], repositories=repositories, operations=['edit', 'review'], indexers={})
        registration = root / 'workspace.json'
        registration.write_text(json.dumps(workspace))
        store = root / 'store'
        subprocess.run([args.manager.resolve(), store, 'register', registration], check=True, stdout=subprocess.DEVNULL)
        revision = 'sha256:' + hashlib.sha256(json.dumps([1, 'smoke', 'host', sorted(repositories, key=lambda r: r['name'])], separators=(',', ':')).encode()).hexdigest()
        binding = dict(schemaVersion=1, workspaceId='smoke', hostId='host', environment='dev', runnerId='runner', membershipRevision=revision,
                       authorizationRevision=1, subjects=['steve'], agents=['codex'], intents=['inspect', 'implement'])
        config = root / 'runner-workspace.json'
        config.write_text(json.dumps(dict(store=str(store), bindings=[binding])))
        config.chmod(0o600)
        request = dict(schemaVersion=1, requestId=str(uuid.uuid4()), workspaceId='smoke', expectedMembershipRevision=revision,
                       task=dict(kind='new', description='Read three repositories'), intent='inspect', expectedCheckpointDigest=None,
                       instruction='Use task_workspace to read README.md in backend, frontend and docs. Return their three exact marker values. Do not modify files.')
        if args.implement:
            request['intent'] = 'implement'
            request['instruction'] = request['instruction'].replace(' Do not modify files.', '')
            request['instruction'] += ' Then append SMOKE_EDIT_OK on its own line to backend/README.md using the digest from read. Include all three original markers and SMOKE_EDIT_OK in your final response.'
        identity = dict(executionId=str(uuid.uuid4()), leaseId=str(uuid.uuid4()), fencingToken=1, transportNonce=uuid.uuid4().hex)
        hello = dict(type='hello', identity=identity, expected_capability_digest=caps['capabilityDigest'])
        start = dict(type='start', session_id=str(uuid.uuid4()), turn_id=str(uuid.uuid4()), action_attempt_id=str(uuid.uuid4()), policy_digest='sha256:'+'a'*64,
                     deadline_ms=120000, input=dict(workspaceSpec=dict(request=request, binding=binding, subject='steve', agentId='codex'), adapterContract=contract, adapterQualification=profile['qualification']))
        env = dict(os.environ, LIGHT_WORKSPACE_CONFIG=str(config), LIGHT_CODEX_EXECUTABLE=str(args.codex.resolve()), LIGHT_CODEX_HOME=str(args.codex_home.resolve()))
        with tempfile.TemporaryFile(mode='w+') as errors:
            process = subprocess.Popen([worker], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=errors, text=True, env=env)
            try:
                process.stdin.write(json.dumps(hello)+'\n'+json.dumps(start)+'\n')
                process.stdin.flush()
                lines = queue.Queue()
                def read_lines():
                    for line in process.stdout: lines.put(line)
                    lines.put(None)
                threading.Thread(target=read_lines, daemon=True).start()
                deadline = time.monotonic()+130
                while time.monotonic()<deadline:
                    try: line = lines.get(timeout=1)
                    except queue.Empty: continue
                    if not line:
                        errors.seek(0)
                        raise RuntimeError('Worker exited: '+errors.read()[-2000:])
                    payload=json.loads(line)['payload']
                    if payload['type']=='progress': print(payload['message'], flush=True)
                    if payload['type']=='terminal':
                        assert payload['class']=='success', payload
                        answer=payload['output']['finalMessage']
                        assert all(marker in answer for marker in ['BACKEND-42','FRONTEND-42','DOCS-42']), answer
                        if args.implement:
                            task = json.loads(next(store.rglob('task.json')).read_text())
                            checkout = next(c for c in task['checkouts'] if c['repository'] == 'backend')
                            assert 'SMOKE_EDIT_OK' in (Path(checkout['path']) / 'README.md').read_text()
                            assert task['state'] == 'ready'
                        print(json.dumps({'status':'passed' ,'result':payload['output']['workspace'],'answer':answer}, indent=2))
                        return
                raise TimeoutError('Workspace smoke exceeded deadline')
            finally:
                process.terminate()
                process.wait(timeout=10)


if __name__ == '__main__':
    main()
