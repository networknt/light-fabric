#!/usr/bin/env python3
"""Opt-in real native Codex thread continuation gate (two paid/included turns)."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import queue
import re
import shutil
import subprocess
import tempfile
import threading
import uuid


def digest(value):
    if not isinstance(value, bytes):
        value = json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=False).encode()
    return 'sha256:' + hashlib.sha256(value).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--codex', required=True, type=Path)
    parser.add_argument('--worker', type=Path, default=Path('target/debug/light-agent-worker'))
    parser.add_argument('--codex-home', type=Path, default=Path.home() / '.codex')
    parser.add_argument('--model', default='gpt-6-astra')
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    source = (root / 'crates/coding-agent-runtime/src/lib.rs').read_text()
    def constant(name):
        return re.search(r'pub const ' + name + r': &str\s*=\s*"([^"]+)";', source).group(1)
    codex, worker = args.codex.resolve(), args.worker.resolve()
    assert digest(codex.read_bytes()) == constant('CODEX_APP_SERVER_BINARY_DIGEST'), 'Unqualified Codex binary'
    capabilities = json.loads(subprocess.check_output([str(worker), 'print-capabilities']))
    evidence = json.loads((root / 'contracts/coding-adapters/codex-app-server-v1-qualification.json').read_text())
    contract = dict(schemaVersion=1, adapterId='codex-app-server-v1', adapterVersion=constant('CODEX_APP_SERVER_VERSION'),
                    adapterProtocolVersion=constant('CODEX_APP_SERVER_PROTOCOL_VERSION'), actionKind='coding.codex-app-server-v1',
                    compatibilityDigest=digest(b'thread-smoke'), imageDigest=digest(worker.read_bytes()),
                    capabilityDigest=capabilities['capabilityDigest'], templateId='coding-codex-app-server-v1', templateVersion=1,
                    templateDigest=digest(b'thread-smoke-template'), executable='/usr/local/bin/codex',
                    binaryDigest=constant('CODEX_APP_SERVER_BINARY_DIGEST'), schemaDigest=constant('CODEX_APP_SERVER_SCHEMA_DIGEST'),
                    requiredFeatures=['codex-app-server-v1'])
    qualification = dict(schemaVersion=1, adapterId=contract['adapterId'], adapterVersion=contract['adapterVersion'],
                         status='qualified', evaluatedDimensions=sorted(evidence['evaluatedDimensions']),
                         contractDigest=digest(contract), evidenceDigest=constant('CODEX_APP_SERVER_QUALIFICATION_EVIDENCE_DIGEST'))
    with tempfile.TemporaryDirectory(prefix='coding-thread-smoke-') as directory:
        work = Path(directory)
        # Test-only credential copy, isolated from the owner's actual thread/config state.
        home = work / 'codex-home'
        home.mkdir(mode=0o700)
        shutil.copyfile(args.codex_home / 'auth.json', home / 'auth.json')
        (home / 'auth.json').chmod(0o600)
        (home / 'config.toml').write_text('model = ' + json.dumps(args.model) + '\n')
        repository = work / 'source'
        repository.mkdir()
        def git(*command):
            return subprocess.check_output(['git', '-C', str(repository), *command], stderr=subprocess.PIPE).decode().strip()
        git('init', '--initial-branch=main')
        (repository / 'README.md').write_text('# Thread smoke\n')
        git('add', 'README.md')
        git('-c', 'user.name=Smoke', '-c', 'user.email=smoke@example.invalid', 'commit', '-m', 'base')
        base = git('rev-parse', 'HEAD')
        bundle = work / 'repository.bundle'
        git('bundle', 'create', str(bundle), '--all')
        manifest = dict(schemaVersion=1, materializerId='coding', materializerVersion=1, productProfile='coding',
                        runtimeCompatibility=contract['compatibilityDigest'], packages=[], effectiveInstructions=[],
                        allowedTools=[], writableRoots=['/workspace/repository'])
        control = dict(runnerId='thread-smoke-runner', sessionRef=str(uuid.uuid4()), stageId='implementation-1', mode='new', closeAfterTurn=False)
        spec = dict(thread=control, repositoryDigest=digest(bundle.read_bytes()), baseRevision=base, workspaceRoot='/workspace/repository',
                    prompt='', modelAlias='coding-implementer', authenticationProfile='personal-subscription', role='implement',
                    roleProfile=dict(profileId='coding-implement-v1', modelAlias='coding-implementer', workspaceAuthority='bounded-write'),
                    reviewInput=None, remediation=None, materializationManifestDigest=digest(manifest),
                    writableRoots=['/workspace/repository'], allowedTools=['fs.read', 'fs.write', 'process.exec'], maximumPatchBytes=65536, maximumChangedFiles=1)
        staged = dict(inputId=str(uuid.uuid4()), sourceDigest=spec['repositoryDigest'], localPath=str(bundle), mountTarget='/inputs/repository.bundle',
                      mediaType='application/x-git-bundle', size=bundle.stat().st_size, readOnly=True, executable=False, mountOptions=['ro'])
        def run(prompt):
            spec['prompt'] = prompt
            env = dict(os.environ, LIGHT_CODEX_HOME=str(home), LIGHT_CODEX_EXECUTABLE=str(codex))
            with tempfile.TemporaryFile(mode='w+') as errors:
                process = subprocess.Popen([str(worker)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=errors, text=True, env=env)
                frames = queue.Queue()
                def read_frames():
                    for line in process.stdout:
                        frames.put(json.loads(line))
                    frames.put(None)
                threading.Thread(target=read_frames, daemon=True).start()
                def send(value):
                    process.stdin.write(json.dumps(value) + '\n')
                    process.stdin.flush()
                send(dict(type='hello', identity=dict(executionId=str(uuid.uuid4()), leaseId=str(uuid.uuid4()), fencingToken=1,
                         transportNonce=uuid.uuid4().hex), expected_capability_digest=capabilities['capabilityDigest']))
                send(dict(type='start', session_id=str(uuid.uuid4()), turn_id=str(uuid.uuid4()), action_attempt_id=str(uuid.uuid4()),
                          policy_digest=digest(b'policy'), input=dict(codingSpec=spec, materializationManifest=manifest,
                          adapterContract=contract, adapterQualification=qualification, runtimeStagedInputs=[staged], threadScope=digest(b'workflow-owner-scope'))))
                patch, usage = None, None
                try:
                    while True:
                        frame = frames.get(timeout=180)
                        if frame is None:
                            errors.seek(0)
                            raise RuntimeError('worker exited: ' + errors.read()[-4000:])
                        payload = frame['payload']
                        if payload['type'] == 'coding-patch':
                            patch = payload['patch']
                        if payload['type'] == 'usage':
                            usage = payload
                        if payload['type'] == 'terminal':
                            assert payload['class'] == 'success', payload
                            print(json.dumps(dict(mode=control['mode'], threadId=payload['output'].get('threadId'), usage=usage)), flush=True)
                            return payload['output'], patch
                finally:
                    process.stdin.close()
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
        marker = 'marker-' + uuid.uuid4().hex
        first, patch = run('Remember this conversation marker: ' + marker + '. Do not write that marker to a file yet. Append exactly the line FIRST to README.md, run git diff --check, and finish. Do not commit.')
        assert '+FIRST' in patch, patch
        control.update(mode='resume', expectedCheckpoint=first['codingThread']['checkpoint'])
        second, patch = run('Continue the preceding task. README.md must still contain FIRST. Append the exact conversation marker I gave you in the previous turn as a new line in README.md. Run git diff --check. Do not commit.')
        assert first['threadId'] == second['threadId'], 'Native thread changed'
        assert '+FIRST' in patch and '+' + marker in patch, 'Conversation or patch continuity failed'
        control.update(mode='close', expectedCheckpoint=second['codingThread']['checkpoint'])
        closed, patch = run('Close this completed workflow stage.')
        assert closed['codingThread']['state'] == 'CLOSED' and patch is None
        print('Native worker new/resume/close passed: same Codex thread, remembered context, restored patch, separate worker processes.', flush=True)


if __name__ == '__main__':
    main()
