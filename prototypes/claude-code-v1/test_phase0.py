import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
import uuid

import phase0

LIMITS = {'frameBytes': 4096, 'outputBytes': 16384, 'stderrBytes': 2048, 'timeoutSeconds': 2}
SID = str(uuid.uuid4())


def stream(*extra):
    return b'\n'.join(json.dumps(v).encode() for v in [
        {'type': 'system', 'subtype': 'init', 'session_id': SID, 'model': 'claude-test'},
        *extra,
        {'type': 'result', 'subtype': 'success', 'session_id': SID, 'is_error': False,
         'result': 'marker', 'usage': {}, 'permission_denials': []}]) + b'\n'


class ProbeTests(unittest.TestCase):
    def test_manifest_and_live_evidence_cannot_promote_adapter(self):
        import hashlib
        contract = json.loads(phase0.CONTRACT.read_text())
        evidence = json.loads((phase0.CONTRACT.parent / 'phase0-live-evidence.json').read_text())
        self.assertEqual(contract['status'], 'prototype-only')
        self.assertFalse(contract['productionQualified'])
        self.assertFalse(evidence['productionQualified'])
        self.assertEqual(evidence['contractSha256'], hashlib.sha256(phase0.CONTRACT.read_bytes()).hexdigest())
        self.assertEqual(evidence['technicalStatus'], 'passed')
        self.assertEqual(len(evidence['cases']), 4)
        for case in evidence['cases']:
            self.assertEqual(case['observedModel'], contract['models'][evidence['requestedModel']])
            self.assertEqual(case['hookObserved'], case['permissionSource'] == 'claude-cli')

    def test_checked_in_synthetic_fixture(self):
        fixture = phase0.CONTRACT.parent / 'fixtures/new-success.jsonl'
        text, evidence = phase0.parse_turn(fixture.read_bytes(), 0,
            '019a0000-0000-7000-8000-000000000001', LIMITS)
        self.assertEqual(text, 'synthetic-marker')
        self.assertEqual(evidence['permissionMode'], 'dontAsk')

    def test_authentication_redacts_private_fields(self):
        raw = json.dumps({'loggedIn': True, 'authMethod': 'claude.ai', 'apiProvider': 'firstParty',
                          'email': 'secret', 'orgId': 'secret'})
        self.assertEqual(phase0.auth_class(raw, 0), 'personal-subscription')
        for changes in ({'authMethod': 'api_key'}, {'loggedIn': False}, {'apiProvider': 'bedrock'}):
            value = json.loads(raw)
            value.update(changes)
            with self.assertRaises(phase0.ProbeError):
                phase0.auth_class(json.dumps(value), 0)

    def test_inheritance_has_no_permission_override(self):
        args = phase0.launch_args('/bin/claude', 'claude-cli', SID, 'sonnet', True)
        self.assertIn('--resume', args)
        self.assertNotIn('--safe-mode', args)
        self.assertNotIn('--permission-mode', args)
        self.assertNotIn('--tools', args)
        self.assertNotIn('--continue', args)
        self.assertIn('--safe-mode', phase0.launch_args('/bin/claude', 'agent-policy', SID, 'sonnet'))

    def test_environment_rejects_conflicting_auth_and_drops_secrets(self):
        with self.assertRaises(phase0.ProbeError):
            phase0.personal_environment({'ANTHROPIC_API_KEY': 'secret'})
        self.assertEqual(phase0.personal_environment({'HOME': '/home/test', 'SECRET': 'secret'}),
                         {'HOME': '/home/test'})

    def test_valid_stream_excludes_transcripts(self):
        text, report = phase0.parse_turn(stream({'type': 'assistant', 'message': 'secret'}), 0, SID, LIMITS)
        self.assertEqual(text, 'marker')
        self.assertNotIn('secret', json.dumps(report))
        self.assertTrue(report['usagePresent'])

    def test_rejects_truncation_duplicate_result_and_wrong_identity(self):
        for raw in (b'{', stream().splitlines()[0], stream() + stream(),
                    stream().replace(SID.encode(), str(uuid.uuid4()).encode()),
                    stream().replace(b'"success"', b'"error_during_execution"'),
                    stream().replace(b'"is_error": false', b'"is_error": true')):
            with self.subTest(raw=raw[:25]), self.assertRaises(phase0.ProbeError):
                phase0.parse_turn(raw, 0, SID, LIMITS)
        with self.assertRaises(phase0.ProbeError):
            phase0.parse_turn(stream(), 1, SID, LIMITS)

    def test_frame_bound_and_stderr_flood(self):
        with tempfile.TemporaryDirectory() as cwd:
            for program in ('print("x"*5000)', 'import sys; sys.stderr.write("x"*5000)'):
                with self.assertRaises(phase0.ProbeError):
                    phase0.run_bounded([sys.executable, '-c', program], cwd, os.environ, LIMITS)

    def test_deadline_also_covers_child_holding_pipe(self):
        with tempfile.TemporaryDirectory() as cwd:
            program = 'import os,time; pid=os.fork(); time.sleep(10) if pid == 0 else None'
            with self.assertRaisesRegex(phase0.ProbeError, 'deadline'):
                phase0.run_bounded([sys.executable, '-c', program], cwd, os.environ,
                                   {**LIMITS, 'timeoutSeconds': 0.2})

    def test_digest_mismatch_fails_before_executing_binary(self):
        contract = json.loads(phase0.CONTRACT.read_text())
        with self.assertRaisesRegex(phase0.ProbeError, 'digest mismatch'):
            phase0.preflight(Path(sys.executable), contract, {}, '/tmp')


if __name__ == '__main__':
    unittest.main()
