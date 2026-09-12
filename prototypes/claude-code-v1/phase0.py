#!/usr/bin/env python3
"""Native feasibility probe only; never advertises a production worker capability."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import selectors
import signal
import subprocess
import tempfile
import time
import uuid

ROOT = Path(__file__).resolve().parents[2]
CONTRACT = ROOT / 'contracts/claude-code/v2.1.269/phase0.json'


class ProbeError(Exception):
    """Only fixed, non-sensitive diagnostics may escape the probe."""


def require(condition, message):
    if not condition:
        raise ProbeError(message)


def run_bounded(argv, cwd, env, limits, prompt=''):
    """Drain both pipes without unbounded communicate() buffering; kill the group."""
    child = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.PIPE,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                             start_new_session=True)
    data = {'stdout': bytearray(), 'stderr': bytearray()}
    deadline = time.monotonic() + limits['timeoutSeconds']
    try:
        child.stdin.write(prompt.encode())
        child.stdin.close()
        with selectors.DefaultSelector() as selector:
            selector.register(child.stdout, selectors.EVENT_READ, 'stdout')
            selector.register(child.stderr, selectors.EVENT_READ, 'stderr')
            while selector.get_map():
                require(time.monotonic() < deadline, 'process deadline exceeded')
                for key, _ in selector.select(min(0.1, max(0, deadline - time.monotonic()))):
                    chunk = os.read(key.fileobj.fileno(), 65536)
                    if not chunk:
                        selector.unregister(key.fileobj)
                        continue
                    name = key.data
                    data[name].extend(chunk)
                    cap = limits['outputBytes'] if name == 'stdout' else limits['stderrBytes']
                    require(len(data[name]) <= cap, name + ' byte limit exceeded')
                    if name == 'stdout':
                        require(all(len(line) <= limits['frameBytes'] for line in data[name].split(b'\n')),
                                'stdout frame limit exceeded')
        remaining = deadline - time.monotonic()
        require(remaining > 0, 'process deadline exceeded')
        code = child.wait(timeout=remaining)
        return code, bytes(data['stdout'])  # stderr is deliberately never emitted
    except (subprocess.TimeoutExpired, BrokenPipeError):
        raise ProbeError('process interrupted or deadline exceeded') from None
    finally:
        # Also kill descendants that closed their inherited pipes before leader exit.
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        child.wait()
        for pipe in (child.stdin, child.stdout, child.stderr):
            pipe.close()


def personal_environment(source=None):
    source = os.environ if source is None else source
    forbidden = ('ANTHROPIC_API_KEY', 'ANTHROPIC_AUTH_TOKEN', 'ANTHROPIC_BASE_URL',
                 'CLAUDE_CODE_OAUTH_TOKEN', 'CLAUDE_CODE_USE_BEDROCK',
                 'CLAUDE_CODE_USE_VERTEX', 'CLAUDE_CODE_USE_FOUNDRY',
                 'CLAUDE_CODE_SKIP_PROMPT_HISTORY', 'CLAUDE_CODE_SIMPLE',
                 'CLAUDE_CODE_SAFE_MODE')
    require(not any(source.get(key) for key in forbidden), 'incompatible authentication/configuration environment')
    # Preserve native configuration directory intentionally, but no arbitrary provider env.
    keys = ('HOME', 'PATH', 'LANG', 'LC_ALL', 'TERM', 'XDG_CONFIG_HOME',
            'XDG_DATA_HOME', 'XDG_CACHE_HOME', 'CLAUDE_CONFIG_DIR',
            'DBUS_SESSION_BUS_ADDRESS', 'XDG_RUNTIME_DIR', 'SSL_CERT_FILE', 'SSL_CERT_DIR')
    return {key: source[key] for key in keys if key in source}


def auth_class(raw, code):
    try:
        value = json.loads(raw)
    except (ValueError, UnicodeError):
        raise ProbeError('invalid native authentication status') from None
    require(isinstance(value, dict) and code == 0 and value.get('loggedIn') is True
            and value.get('authMethod') == 'claude.ai'
            and value.get('apiProvider') == 'firstParty', 'native subscription authentication required')
    # Never return the email, organization ID, account ID, or raw response.
    return 'personal-subscription'


def parse_turn(raw, code, session_id, limits):
    require(len(raw) <= limits['outputBytes'], 'stdout byte limit exceeded')
    events = []
    for line in raw.splitlines():
        require(0 < len(line) <= limits['frameBytes'], 'invalid event size')
        try:
            value = json.loads(line)
        except (ValueError, UnicodeError):
            raise ProbeError('invalid JSON event') from None
        require(isinstance(value, dict) and isinstance(value.get('type'), str), 'invalid event envelope')
        if 'session_id' in value:
            require(value['session_id'] == session_id, 'native session identity mismatch')
        events.append(value)
    initial = [v for v in events if v.get('type') == 'system' and v.get('subtype') == 'init']
    results = [v for v in events if v.get('type') == 'result']
    require(len(initial) == 1 and len(results) == 1, 'missing or duplicate initialization/result')
    result = results[0]
    require(events[-1] is result and code == 0 and result.get('subtype') == 'success'
            and result.get('is_error') is False, 'native turn did not complete successfully')
    require(result.get('session_id') == session_id and initial[0].get('session_id') == session_id,
            'native session identity missing')
    require(isinstance(result.get('result'), str), 'native result text missing')
    model = initial[0].get('model')
    require(isinstance(model, str) and model.startswith('claude-') and len(model) < 128,
            'native model metadata missing')
    # Store schema evidence, not transcript text/tool arguments or account metadata.
    shapes = sorted({(v['type'], tuple(sorted(v.keys()))) for v in events})
    return result['result'], {
        'observedModel': model,
        'permissionMode': initial[0].get('permissionMode') if initial[0].get('permissionMode') in
                          ('default', 'manual', 'dontAsk', 'acceptEdits', 'auto', 'plan', 'bypassPermissions') else 'unknown',
        'mcpServerCount': len(initial[0].get('mcp_servers', [])),
        'pluginCount': len(initial[0].get('plugins', [])),
        'eventShapes': [{'type': kind, 'keys': list(keys)} for kind, keys in shapes],
        'usagePresent': isinstance(result.get('usage'), dict),
        'permissionDenialCount': len(result.get('permission_denials', [])),
    }


def launch_args(binary, permission_source, session_id, model, resume=False):
    require(permission_source in ('agent-policy', 'claude-cli'), 'unknown permission source')
    args = [str(binary), '-p', '--output-format', 'stream-json', '--verbose',
            '--include-partial-messages', '--permission-prompts', 'none',
            '--model', model, '--resume' if resume else '--session-id', session_id]
    if permission_source == 'agent-policy':
        args += ['--safe-mode', '--permission-mode', 'dontAsk', '--tools', '']
    return args


def preflight(binary, contract, env, cwd):
    expected = contract['binary']
    require(platform.system() == expected['system'] and platform.machine() == expected['machine'],
            'unqualified native platform')
    with binary.open('rb') as stream:
        digest = hashlib.file_digest(stream, 'sha256').hexdigest()
    require(digest == expected['sha256'], 'native binary digest mismatch')
    code, raw = run_bounded([str(binary), '--version'], cwd, env, contract['limits'])
    require(code == 0 and raw.decode().strip() == expected['version'], 'native binary version mismatch')
    code, raw = run_bounded([str(binary), '--help'], cwd, env, contract['limits'])
    require(code == 0 and all(flag.encode() in raw for flag in contract['requiredFlags']),
            'required native CLI flag missing')
    code, raw = run_bounded([str(binary), 'auth', 'status'], cwd, env, contract['limits'])
    return auth_class(raw, code)


def live_probe(binary, model, contract, report):
    env = personal_environment()
    with tempfile.TemporaryDirectory(prefix='light-claude-phase0-') as directory:
        cwd = Path(directory)
        report['authenticationClass'] = preflight(binary, contract, env, cwd)
        require(model in contract['models'], 'model has no pinned qualification mapping')
        settings = cwd / '.claude'
        settings.mkdir()
        # Harmless canary is intentionally discovered only in the native source profile.
        # No existing user settings or credentials are changed/copied.
        (settings / 'settings.json').write_text(json.dumps({'model': 'haiku', 'permissions': {'defaultMode': 'dontAsk'},
                                                               'hooks': {'SessionStart': [{
            'hooks': [{'type': 'command', 'command': 'printf phase0 > phase0-hook-marker'}]
        }]}}))
        marker_file = cwd / 'phase0-hook-marker'
        for source in ('agent-policy', 'claude-cli'):
            session_id = str(uuid.uuid4())
            token = 'phase0-' + uuid.uuid4().hex
            for resume in (False, True):
                if marker_file.exists():
                    marker_file.unlink()
                # The second prompt deliberately does not contain the random token.
                prompt = ('Remember this marker for this conversation: ' + token + '. Reply only with the marker. Do not use tools.'
                          if not resume else 'Reply only with the marker from my previous message. Do not use tools.')
                code, raw = run_bounded(launch_args(binary, source, session_id, model, resume),
                                        cwd, env, contract['limits'], prompt)
                text, evidence = parse_turn(raw, code, session_id, contract['limits'])
                require(text.strip() == token, 'conversation-only marker recall failed')
                require(evidence['observedModel'] == contract['models'][model], 'native model selection mismatch')
                require(evidence['permissionMode'] == 'dontAsk', 'native permission mode mismatch')
                if source == 'agent-policy':
                    require(evidence['mcpServerCount'] == 0 and evidence['pluginCount'] == 0,
                            'unexpected native customization in managed profile')
                require(marker_file.exists() == (source == 'claude-cli'), 'configuration hook discovery differs from profile')
                require(evidence['permissionDenialCount'] == 0, 'unexpected permission denial')
                report['cases'].append({'permissionSource': source, 'operation': 'resume' if resume else 'new',
                                        'status': 'passed', 'markerRecall': True,
                                        'hookObserved': marker_file.exists(), **evidence})
                print(source + '/' + ('resume' if resume else 'new') + ': passed', flush=True)
        report['technicalStatus'] = 'passed'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--claude', type=Path, required=True)
    parser.add_argument('--model', default='sonnet')
    parser.add_argument('--report', type=Path, required=True)
    parser.add_argument('--live', action='store_true', help='Consume native subscription usage; creates native probe sessions')
    args = parser.parse_args()
    contract = json.loads(CONTRACT.read_text())
    report = {'schemaVersion': 1, 'recordedAtUtc': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
              'contractSha256': hashlib.sha256(CONTRACT.read_bytes()).hexdigest(), 'adapterId': 'claude-code-v1', 'productionQualified': False,
              'binary': contract['binary'], 'requestedModel': args.model, 'cases': [],
              'technicalStatus': 'not-run', 'distributionEligibility': 'unresolved'}
    code = 0
    try:
        require(args.live, 'live opt-in required')
        require(args.model and len(args.model) < 128 and all(c.isalnum() or c in '-._' for c in args.model),
                'invalid native model selection')
        live_probe(args.claude.resolve(strict=True), args.model, contract, report)
    except (ProbeError, OSError, ValueError, TypeError, RecursionError) as error:
        report['technicalStatus'] = 'failed'
        report['error'] = str(error) if isinstance(error, ProbeError) else 'local probe I/O or format failure'
        code = 1
    args.report.parent.mkdir(parents=True, exist_ok=True)
    fd = os.open(args.report, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'w') as stream:
        json.dump(report, stream, indent=2)
        stream.write('\n')
    print(json.dumps({'technicalStatus': report['technicalStatus'], 'productionQualified': False}))
    return code


if __name__ == '__main__':
    raise SystemExit(main())
