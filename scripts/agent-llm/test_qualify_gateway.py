import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import uuid

spec = importlib.util.spec_from_file_location('qualification', Path(__file__).with_name('qualify_gateway.py'))
qualification = importlib.util.module_from_spec(spec)
spec.loader.exec_module(qualification)

class QualificationTests(unittest.TestCase):
    def run_probe(self, *, wrong_policy=False, provider_attempt=False):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config = {'endpoint':'https://gateway.example/v1/chat/completions', 'alias':'assigned', 'unassignedAlias':'other-agent', 'agentPolicyDigest':'sha256:'+'a'*64, 'requireWorkload':True}
            for name in ['hostId','agentDefId','userId','workloadClientId']:
                config[name] = str(uuid.uuid4())
            (root/'config.json').write_text(json.dumps(config))
            for name in ['user','workload']:
                (root/name).write_text(name+'-credential'); (root/name).chmod(0o600)
            report = root/'report.json'; report.write_text('{"status":"PASS_FROM_OLD_RUN"}')
            ids=[]
            def request(_opener,_endpoint,_user,workload,alias,correlation):
                ids.append(correlation)
                if workload in [None,'invalid-phase4-token']: return 401,None
                return (200,{'choices':[{}]}) if alias=='assigned' else (404,None)
            def rows(_ids):
                result=[]
                for index, correlation in enumerate(ids):
                    auth = {'correlationId':correlation,'userId':config['userId'],'hostId':config['hostId']}
                    if index==0:
                        auth.update(agentDefId=config['agentDefId'],workloadClientId=config['workloadClientId'],policyDigest='wrong' if wrong_policy else config['agentPolicyDigest'],userAccessDecision='allowed',agentAssignmentDecision='allowed')
                    result.append({'request_id':correlation,'event_kind':'request_finished','public_alias':'assigned','generation':1,'snapshot_digest':'sha256:'+'b'*64,'authorization_context':auth})
                if provider_attempt:
                    result.append({**result[1], 'event_kind':'attempt_started'})
                return result
            args=['qualify','--config',str(root/'config.json'),'--user-token-file',str(root/'user'),'--workload-token-file',str(root/'workload'),'--ca-file',str(root/'ca'),'--report',str(report)]
            with patch('sys.argv',args), patch.object(qualification.ssl,'create_default_context'), patch.object(qualification.urllib.request,'build_opener'), patch.object(qualification,'request',side_effect=request), patch.object(qualification,'audit_rows',side_effect=rows):
                if wrong_policy or provider_attempt:
                    with self.assertRaises(RuntimeError): qualification.main()
                    self.assertEqual(json.loads(report.read_text())['status'],'IN_PROGRESS')
                else:
                    qualification.main()
                    data=json.loads(report.read_text()); self.assertEqual(data['status'],'GATEWAY_ADMISSION_PASSED'); self.assertEqual(data['browserQualification'],'NOT_RUN'); self.assertEqual(len(data['cases']),5)
                    self.assertNotIn('user-credential',report.read_text()); self.assertNotIn('workload-credential',report.read_text())
    def test_requires_audit_evidence_and_never_claims_browser_qualification(self): self.run_probe()
    def test_policy_mismatch_cannot_reuse_an_old_pass(self): self.run_probe(wrong_policy=True)
    def test_provider_attempt_on_denial_fails(self): self.run_probe(provider_attempt=True)
    def test_secrets_require_owner_only_permissions(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'token'; path.write_text('secret'); path.chmod(0o644)
            with self.assertRaises(ValueError): qualification.secret(path)

if __name__ == '__main__': unittest.main()
