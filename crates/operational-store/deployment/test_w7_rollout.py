"""Bounded mocks/static checks. Never instantiate a production adapter."""
import copy
from datetime import datetime, timedelta, timezone
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from contextlib import redirect_stdout, redirect_stderr
from unittest.mock import patch
import uuid

from w7_rollout import (Coordinator, Refusal, inventory, preflight_sql,
                        serialized, verify_assets, validate_plan, cli)

HERE = Path(__file__).resolve().parent
ASSETS = HERE.parent / 'release'
NOW = datetime(2026, 10, 3, 16, tzinfo=timezone.utc)
IMAGE = 'sha256:' + 'a' * 64
INSTANCE = str(uuid.UUID(int=1))


def fixture():
    plan = dict(version=1, deployment_id='bounded-fixture', freshness_seconds=90,
                gates=[dict(id='portal',kind='portal',database='configserver',schema='public',pgservice='fixture_portal'),
                       dict(id='ops',kind='operational',database='operations',schema='workflow_ops',pgservice='fixture_ops',scope_root_id=str(uuid.UUID(int=2)))],
                placements=[dict(id='local',adapter='owner-orchestrator-snapshot',gate_ids=['portal','ops'],services={
                    'workflow':dict(role='workflow',minimum_instances=1,maximum_instances=2,images=[IMAGE],binary_version='fixture-v2',gate_id='ops',supported_profiles=['cel-workflow-v1','cel-workflow-v2'],admits_profiles=['cel-workflow-v1','cel-workflow-v2']),
                    'controller':dict(role='controller',minimum_instances=1,maximum_instances=1,images=[IMAGE]),
                    'portal':dict(role='portal-admission',minimum_instances=1,maximum_instances=1,images=[IMAGE])})])
    instances = [dict(id=name,image_id=IMAGE,service=name,state='running',started_at=(NOW-timedelta(seconds=20)).isoformat())
                 for name in ['workflow','controller','portal']]
    readiness = {i['id']:dict(image_id=IMAGE,started_at=i['started_at'],checked_at=NOW.isoformat(),
                              contracts=['execution-results-page-v1'] if i['id']=='controller' else
                              ['independent-admission','validator-build-allowlist','receipt-recovery'],
                              authenticated_page_probe=dict(status=200,envelope='items,nextCursor',route='/internal/execution/results/page'))
                 for i in instances if i['id']!='workflow'}
    evidence = dict(version=1,deployment_id=plan['deployment_id'],plan_sha256='fixture-plan',captured_at=NOW.isoformat(),
                    owner_review='fixture-owner-review',quiescence_operation_id='fixture-quiescence',
                    placements=[dict(id='local',captured_at=NOW.isoformat(),all_instances_accounted_for=True,
                                     external_writers_quiesced=True,activation_operators_excluded=True,authority_reference='mock-only',
                                     instances=instances,readiness=readiness)])
    return plan,evidence


class MockBackend:
    plan_digest = 'fixture-plan'
    def __init__(self):
        self.values = {'portal':'OFF','ops':'OFF'}
        self.claims = {'portal':True,'ops':True}
        self.calls = []
        self.active = set()
        self.failure = None
        self.row = dict(instance_id=INSTANCE,binary_version='fixture-v2',supported_profiles=['cel-workflow-v1','cel-workflow-v2'],
                        admits_profiles=['cel-workflow-v1','cel-workflow-v2'],age_seconds=1,heartbeat_ts=(NOW-timedelta(seconds=1)).isoformat())
        self.change_inventory=False
        self.inventory_count=0
    def now(self): return NOW
    def state(self,g):
        if self.failure == ('state',g['id']): raise RuntimeError('SENTINEL_SECRET_DATABASE_URL')
        return self.values[g['id']]
    def coverage(self,plan,evidence):
        self.calls.append('coverage')
        if not evidence.get('quiescence_operation_id') or not all(p.get('external_writers_quiesced') for p in evidence['placements']):
            raise Refusal('QUIESCENCE_UNPROVEN')
    def instances(self,placement,recorded):
        self.calls.append('inventory')
        self.inventory_count+=1
        rows=copy.deepcopy(recorded['instances'])
        if self.change_inventory and self.inventory_count>1: rows[0]['started_at']=NOW.isoformat()
        return rows
    def workflow_identity(self,placement,instance,recorded):
        if self.failure == ('identity','workflow'): raise Refusal('CORRELATION_MISSING')
        return INSTANCE
    def heartbeat(self,gate_id,identity):
        self.calls.append('heartbeat')
        if self.failure == ('heartbeat','ops'): raise Refusal('HEARTBEAT_MISSING')
        return self.row
    def preflight(self,g,sql):
        self.calls.append('preflight:'+g['id'])
        if self.failure == ('preflight',g['id']): raise Refusal('PREFLIGHT_REFUSED')
        assert 'wf_definition_version_t' in sql and 'chr(92)' in sql and 'ALTER TABLE' not in sql
    def install(self,g):
        self.calls.append('install:'+g['id'])
        if self.failure == ('install',g['id']): raise Refusal('INSTALL_INTERRUPTED')
        self.values[g['id']]='OFF'
    def preinstall(self,g,transition=False):
        self.calls.append('preinstall:'+g['id'])
        if self.failure == ('preinstall',g['id']): raise Refusal('PREINSTALL_REFUSED')
    def schema(self,g,claim_required):
        self.calls.append('schema:'+g['id'])
        if claim_required and not self.claims[g['id']]: raise Refusal('REACTIVATION_REQUIRES_REVIEWED_RESTORATION')
        if self.failure == ('schema',g['id']): raise Refusal('SCHEMA_MISSING')
    def write_startup_marker(self,state):
        (Path(state)/'startup-ready.sha256').write_text('mock-only\n')
    def toggle(self,g,on):
        action='on' if on else 'off'
        self.calls.append(action+':'+g['id'])
        if self.failure == (action,g['id']): raise RuntimeError('SENTINEL_SECRET_PASSWORD')
        self.values[g['id']]='ON' if on else 'OFF'
        if self.failure == ('lost-'+action,g['id']): raise RuntimeError('SENTINEL_SECRET_LOST_REPLY')
    def active_run_check(self,g):
        self.calls.append('active:'+g['id'])
        if g['id'] in self.active: raise Refusal('ACTIVE_NONLEGACY')
        if self.values[g['id']]!='OFF': raise Refusal('ADMISSION_NOT_OFF')
    def claim_present(self,g): return self.claims[g['id']]
    def rollback(self,g):
        self.calls.append('rollback:'+g['id'])
        if self.failure == ('rollback',g['id']): raise Refusal('ROLLBACK_FAILED')
        self.claims[g['id']]=False
        if self.failure == ('lost-rollback',g['id']): raise Refusal('ROLLBACK_REPLY_LOST')


class CoordinatorTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.plan,self.evidence=fixture()
        self.backend=MockBackend()
        self.state=Path(self.temp.name)
        self.c=Coordinator(self.plan,self.backend,ASSETS,self.state,self.evidence)
    def prepared(self):
        self.c.prepare()
        self.backend.calls.clear()
    def test_all_gate_preinstall_refusal(self):
        # A valid earlier gate must not be installed before a later gate's
        # qualification/authority/ledger inputs have been checked.
        for gate in self.plan['gates']:
            with self.subTest(gate=gate['id']):
                self.backend.calls.clear()
                self.backend.failure = ('preinstall', gate['id'])
                with self.assertRaisesRegex(Refusal, 'PREINSTALL_REFUSED'):
                    self.c.prepare()
                self.assertFalse(any(c.startswith('install:') for c in self.backend.calls))
                self.assertFalse((self.state / 'prepared.json').exists())
                self.assertFalse((self.state / 'startup-ready.sha256').exists())
    def test_prepare_then_upgrade_boundary_then_activate_order(self):
        self.c.prepare()
        self.assertEqual(self.backend.calls[:3],['coverage','preflight:portal','preflight:ops'])
        codes=[e['code'] for e in self.c.record['events']]
        self.assertLess(codes.index('PREFLIGHT_1'),codes.index('VERIFY_DIGESTS'))
        self.assertLess(codes.index('VERIFY_DIGESTS'),codes.index('SCHEMA_INTENT'))
        self.assertFalse(any(x.startswith('on:') for x in self.backend.calls))
        self.c.activate()
        calls=self.backend.calls
        self.assertLess(calls.index('on:ops'),calls.index('on:portal'))
        self.assertEqual(self.backend.values,{'portal':'ON','ops':'ON'})
    def test_prepare_already_on_refuses_without_disable_or_preflight(self):
        self.backend.values['ops']='ON'
        with self.assertRaisesRegex(Refusal,'ALREADY_ON'): self.c.prepare()
        self.assertEqual(self.backend.calls,[])
        self.assertEqual(self.backend.values['ops'],'ON')
    def test_unknown_gate_never_reported_off(self):
        self.backend.failure=('state','ops')
        self.assertEqual(self.c.states()['ops'],'UNKNOWN')
        with self.assertRaises(Refusal):self.c.prepare()
        self.assertNotIn('install:portal',self.backend.calls)
    def test_repeated_preparation_is_off_and_never_activates(self):
        self.c.prepare(); self.c.prepare()
        self.assertEqual(self.backend.calls.count('install:ops'),2)
        self.assertEqual(self.backend.values,{'portal':'OFF','ops':'OFF'})
        self.assertTrue((self.state/'startup-ready.sha256').exists())
    def test_partial_preparation_removes_restart_permission(self):
        self.prepared()
        self.backend.failure=('install','ops')
        with self.assertRaises(Refusal): self.c.prepare()
        self.assertFalse((self.state/'startup-ready.sha256').exists())
        self.assertEqual(self.backend.values,{'portal':'OFF','ops':'OFF'})
    def test_recover_preparation_after_interruption(self):
        self.backend.failure=('install','ops')
        with self.assertRaises(Refusal):self.c.prepare()
        self.backend.failure=None
        self.c.prepare()
        self.assertTrue((self.state/'startup-ready.sha256').exists())
    def test_activate_requires_preparation(self):
        with self.assertRaises(Refusal):self.c.activate()
        self.assertFalse(any(x.startswith('on:') for x in self.backend.calls))
    def test_activate_already_on_does_not_toggle(self):
        self.prepared();self.backend.values['portal']='ON'
        with self.assertRaisesRegex(Refusal,'ALREADY_ON'):self.c.activate()
        self.assertEqual(self.backend.calls,[])
    def test_preflight_two_failure_before_any_activation(self):
        self.prepared();self.backend.failure=('preflight','ops')
        with self.assertRaises(Refusal):self.c.activate()
        self.assertFalse(any(x.startswith('on:') for x in self.backend.calls))
    def test_schema_missing_before_activation(self):
        self.prepared();self.backend.failure=('schema','ops')
        with self.assertRaises(Refusal):self.c.activate()
        self.assertFalse(any(x.startswith('on:') for x in self.backend.calls))
    def test_partial_activation_recovers_portal_first(self):
        self.prepared();self.backend.failure=('on','portal')
        with self.assertRaises(RuntimeError):self.c.activate()
        calls=self.backend.calls
        self.assertLess(calls.index('off:portal'),calls.index('off:ops'))
        self.assertEqual(self.backend.values,{'portal':'OFF','ops':'OFF'})
    def test_lost_on_reply_is_read_back_and_recovered(self):
        self.prepared();self.backend.failure=('lost-on','ops')
        with self.assertRaises(RuntimeError):self.c.activate()
        self.assertEqual(self.backend.values,{'portal':'OFF','ops':'OFF'})
        self.assertNotIn('on:portal',self.backend.calls)
    def test_failed_off_preserves_existing_on_and_blocks_downgrade(self):
        self.prepared();self.backend.values={'portal':'ON','ops':'ON'};self.backend.failure=('off','portal')
        with self.assertRaises(Refusal):self.c.rollback()
        self.assertEqual(self.backend.values,{'portal':'ON','ops':'OFF'})
        self.assertFalse(any(x.startswith('rollback:') for x in self.backend.calls))
        self.assertNotIn('DOWNGRADE_ELIGIBLE_REVERIFY_BEFORE_OWNER_ACTION',[x['code'] for x in self.c.record['events']])
    def test_explicit_recovery_after_process_kill_reads_all_gates(self):
        self.backend.values={'portal':'OFF','ops':'ON'}
        self.c.recover_off()
        self.assertEqual(self.backend.calls,['coverage','off:portal','off:ops'])
        self.c.all_off()
    def test_active_nonlegacy_refuses_all_compatibility_removal(self):
        self.prepared();self.backend.active={'ops'}
        with self.assertRaisesRegex(Refusal,'ACTIVE_NONLEGACY'):self.c.rollback()
        self.assertFalse(any(x.startswith('rollback:') for x in self.backend.calls))
        self.c.all_off()
    def test_unqualified_schema_refuses_rollback_before_toggles(self):
        self.prepared();self.backend.failure=('schema','ops')
        with self.assertRaises(Refusal):self.c.rollback()
        self.assertFalse(any(x.startswith(('off:', 'rollback:')) for x in self.backend.calls))
    def test_rollback_order_and_repeat_with_retained_receipts(self):
        self.prepared();self.c.rollback()
        self.assertLess(self.backend.calls.index('off:portal'),self.backend.calls.index('off:ops'))
        self.assertLess(self.backend.calls.index('active:ops'),self.backend.calls.index('rollback:portal'))
        self.assertEqual(self.backend.calls.count('rollback:portal'),1)
        self.c.rollback()
        self.assertEqual(self.backend.calls.count('rollback:portal'),1)
        self.assertEqual(self.backend.calls.count('rollback:ops'),1)
    def test_partial_rollback_can_resume_but_cannot_downgrade(self):
        self.prepared();self.backend.failure=('rollback','ops')
        with self.assertRaises(Refusal):self.c.rollback()
        self.assertFalse(self.backend.claims['portal']);self.assertTrue(self.backend.claims['ops'])
        self.assertNotIn('DOWNGRADE_ELIGIBLE_REVERIFY_BEFORE_OWNER_ACTION',[x['code'] for x in self.c.record['events']])
        self.backend.failure=None;self.c.rollback()
        self.assertFalse(self.backend.claims['ops'])
    def test_lost_rollback_reply_requires_provenance_review(self):
        self.prepared();self.backend.failure=('lost-rollback','portal')
        with self.assertRaises(Refusal):self.c.rollback()
        self.backend.failure=None
        with self.assertRaises(Refusal):self.c.rollback()
        self.assertNotIn('rollback:ops',self.backend.calls)
    def test_reactivation_after_rollback_refuses_without_reinstall(self):
        self.prepared();self.c.rollback();self.backend.calls.clear()
        with self.assertRaisesRegex(Refusal,'RESTORATION'):self.c.prepare()
        self.assertFalse(any(x.startswith('on:') for x in self.backend.calls))
    def test_changed_inventory_blocks_activation(self):
        self.prepared();self.backend.change_inventory=True
        with self.assertRaises(Refusal):self.c.activate()
        self.assertNotIn('on:ops',self.backend.calls)
    def test_missing_quiescence_stops_before_preflight(self):
        self.evidence['placements'][0]['external_writers_quiesced']=False
        with self.assertRaises(Refusal):self.c.prepare()
        self.assertNotIn('preflight:portal',self.backend.calls)
    def test_restart_allows_on_but_requires_controller_page(self):
        self.prepared();self.backend.values={'portal':'ON','ops':'ON'}
        self.c.restart_check()
        self.evidence['placements'][0]['readiness']['controller']['authenticated_page_probe']['status']=404
        with self.assertRaisesRegex(Refusal,'CONTROLLER_PAGE'):self.c.restart_check()
        self.assertFalse(any(x.startswith('on:') for x in self.backend.calls))
    def test_partial_state_and_journal_do_not_expose_secret_failures(self):
        self.prepared();self.backend.failure=('on','portal')
        try:self.c.activate()
        except RuntimeError:pass
        self.assertNotIn('SENTINEL_SECRET',(self.state/'operation.json').read_text())
        self.assertNotIn('SENTINEL_SECRET',json.dumps(self.c.states()))


class InventoryTests(unittest.TestCase):
    def setUp(self):self.plan,self.evidence=fixture();self.backend=MockBackend()
    def run_inventory(self):return inventory(self.plan,self.evidence,self.backend,NOW,'activate')
    def test_role_specific_positive_control(self):self.assertEqual(len(self.run_inventory()),3)
    def test_no_cel_heartbeat_required_for_other_services(self):
        self.run_inventory();self.assertEqual(self.backend.calls.count('heartbeat'),1)
    def test_missing_and_stale_and_future_heartbeat(self):
        for kind in ['missing','stale','future']:
            with self.subTest(kind=kind):
                self.backend=MockBackend()
                if kind=='missing':self.backend.failure=('heartbeat','ops')
                else:self.backend.row['age_seconds']=91 if kind=='stale' else -1
                with self.assertRaises(Refusal):self.run_inventory()
    def test_mismatched_capabilities_and_version(self):
        for key,value in [('binary_version','old'),('supported_profiles',['cel-workflow-v1']),('admits_profiles',[])]:
            with self.subTest(key=key):
                self.backend=MockBackend();self.backend.row[key]=value
                with self.assertRaises(Refusal):self.run_inventory()
    def test_prior_process_heartbeat_refused(self):
        self.backend.row['heartbeat_ts']=(NOW-timedelta(seconds=30)).isoformat()
        with self.assertRaises(Refusal):self.run_inventory()
    def test_process_identity_missing_or_ambiguous(self):
        self.backend.failure=('identity','workflow')
        with self.assertRaises(Refusal):self.run_inventory()
        self.backend.failure=None
        self.evidence['placements'][0]['instances'].append(dict(self.evidence['placements'][0]['instances'][0],id='replica2'))
        with self.assertRaisesRegex(Refusal,'AMBIGUOUS'):self.run_inventory()
    def test_missing_replica(self):
        self.evidence['placements'][0]['instances'].pop(0)
        with self.assertRaises(Refusal):self.run_inventory()
    def test_unknown_image_or_unclassified_instance(self):
        instances=self.evidence['placements'][0]['instances']
        instances[0]['image_id']='sha256:'+'b'*64
        with self.assertRaises(Refusal):self.run_inventory()
        instances[0]['image_id']=IMAGE;instances[0]['service']='unknown'
        with self.assertRaises(Refusal):self.run_inventory()
    def test_missing_placement_and_quiescence(self):
        self.plan['placements'].append(dict(self.plan['placements'][0],id='external'))
        with self.assertRaisesRegex(Refusal,'COVERAGE'):self.run_inventory()
        self.plan['placements'].pop();self.evidence['placements'][0]['activation_operators_excluded']=False
        with self.assertRaisesRegex(Refusal,'QUIESCENCE'):self.run_inventory()
    def test_future_or_stale_evidence(self):
        for age in [-1,91]:
            self.evidence['captured_at']=(NOW-timedelta(seconds=age)).isoformat()
            with self.assertRaises(Refusal):self.run_inventory()
    def test_controller_route_contract_and_page_probe_required(self):
        row=self.evidence['placements'][0]['readiness']['controller']
        row['contracts']=[]
        with self.assertRaises(Refusal):self.run_inventory()
        row['contracts']=['execution-results-page-v1'];row['authenticated_page_probe']['status']=404
        with self.assertRaises(Refusal):self.run_inventory()


class StaticTests(unittest.TestCase):
    def test_cli_injected_mock_failures_never_expose_secrets(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder)/'assets';shutil.copytree(ASSETS,root,ignore=shutil.ignore_patterns('.runtime'))
            plan,evidence=fixture()
            path=Path(folder)/'plan.json';path.write_text(json.dumps(plan))
            backend=MockBackend();backend.plan_digest=hashlib.sha256(path.read_bytes()).hexdigest()
            backend.failure=('install','ops')
            evidence['plan_sha256']=backend.plan_digest
            proof=Path(folder)/'evidence.json';proof.write_text(json.dumps(evidence))
            stdout,stderr=io.StringIO(),io.StringIO()
            with patch('w7_rollout.LiveBackend',lambda *_:backend),redirect_stdout(stdout),redirect_stderr(stderr):
                code=cli(['prepare','--owner-run','--assets',str(root),'--plan',str(path),'--evidence',str(proof),'--state-dir',str(root/'.runtime/w7')])
            self.assertEqual(code,2)
            self.assertNotIn('SENTINEL_SECRET',stdout.getvalue()+stderr.getvalue())
            self.assertIn('"global_off": true',stdout.getvalue())
    def test_cli_unknown_is_not_global_off(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder)/'assets';shutil.copytree(ASSETS,root,ignore=shutil.ignore_patterns('.runtime'))
            plan,_=fixture();path=Path(folder)/'plan.json';path.write_text(json.dumps(plan))
            backend=MockBackend();backend.failure=('state','ops')
            output=io.StringIO()
            with patch('w7_rollout.LiveBackend',lambda *_:backend),redirect_stdout(output):
                code=cli(['status','--owner-run','--assets',str(root),'--plan',str(path),'--state-dir',str(root/'.runtime/w7')])
            self.assertEqual(code,0)
            self.assertIn('"global_off": false',output.getvalue())
            self.assertIn('UNKNOWN',output.getvalue())
            self.assertNotIn('SENTINEL_SECRET',output.getvalue())
    def test_current_assets(self):self.assertEqual(len(verify_assets(ASSETS)[1]),44)
    def test_exact_both_preflight_prefixes(self):
        for kind in ['portal','operational']:
            sql=preflight_sql(ASSETS,{'kind':kind})
            self.assertIn('WORKFLOW_EXPRESSION_RESERVED_KEY_COLLISION',sql)
            self.assertIn('WORKFLOW_EXPRESSION_RESERVED_KEY_AMBIGUITY',sql)
            self.assertNotIn('ALTER TABLE',sql)
    def test_wrong_bundle_and_individual_digest(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder)/'assets';shutil.copytree(ASSETS,root,ignore=shutil.ignore_patterns('.runtime'))
            path=root/'bundle/manifest.json';path.write_text(path.read_text()+' ')
            with self.assertRaises(Refusal):verify_assets(root)
    def test_migration_and_rollback_digest_tampering(self):
        for rel in ['bundle/crates/workflow-store/migrations/workflow-postgres/0025_workflow_operation_receipts.sql','bundle/rollback/0024_workflow_expression_profile.sql']:
            with self.subTest(rel=rel),tempfile.TemporaryDirectory() as folder:
                root=Path(folder)/'assets';shutil.copytree(ASSETS,root,ignore=shutil.ignore_patterns('.runtime'))
                path=root/rel;path.write_text(path.read_text()+'\n')
                with self.assertRaises(Refusal):verify_assets(root)
    def test_altered_portal_rollback_and_adjacent_pins_refuse(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder)/'assets';shutil.copytree(ASSETS,root,ignore=shutil.ignore_patterns('.runtime'))
            relative='portal/rollback/rollback_20261002_01_workflow_expression_profile.sql'
            path=root/relative;path.write_text(path.read_text()+'\n')
            descriptor=root/'w7-assets.json';pins=json.loads(descriptor.read_text())
            pins['files'][relative]=hashlib.sha256(path.read_bytes()).hexdigest()
            descriptor.write_text(json.dumps(pins))
            with self.assertRaisesRegex(Refusal,'DESCRIPTOR_IDENTITY'):verify_assets(root)
    def test_lock_excludes_second_operation(self):
        with tempfile.TemporaryDirectory() as folder:
            with serialized(folder):
                with self.assertRaisesRegex(Refusal,'BUSY'):
                    with serialized(folder):pass
    def test_startup_guard_without_marker_and_changed_assets(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder)/'operations';shutil.copytree(ASSETS,root,ignore=shutil.ignore_patterns('.runtime'))
            state=root/'.runtime/w7';state.mkdir(parents=True)
            command=['bash',str(HERE/'w7-startup-guard.sh'),str(root)]
            self.assertEqual(subprocess.run(command,capture_output=True).returncode,2)
            (state/'prepared.json').write_text('{}\n')
            marker=''.join(hashlib.sha256(p.read_bytes()).hexdigest()+'  '+str(p.relative_to(root))+'\n'
                           for p in [root/'w7-assets.json',root/'bundle/bundle.sha256',state/'prepared.json'])
            (state/'startup-ready.sha256').write_text(marker)
            self.assertEqual(subprocess.run(command,capture_output=True).returncode,0)
            (state/'prepared.json').write_text('{"unfinished":true}\n')
            self.assertEqual(subprocess.run(command,capture_output=True).returncode,2)
    def test_startup_guard_blocks_unfinished_prepare_lock(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder);state=root/'.runtime/w7';state.mkdir(parents=True)
            (state/'startup-ready.sha256').write_text('mock\n')
            with serialized(state):
                result=subprocess.run(['bash',str(HERE/'w7-startup-guard.sh'),str(root)],capture_output=True)
            self.assertEqual(result.returncode,2)
    def test_raw_rollback_retains_off_commit_and_guards(self):
        for path in [ASSETS/'bundle/rollback/0024_workflow_expression_profile.sql',ASSETS/'portal/rollback/rollback_20261002_01_workflow_expression_profile.sql']:
            source=path.read_text()
            self.assertLess(source.index('COMMIT;'),source.index('WORKFLOW_EXPRESSION_ACTIVE_V2_PROCESS'))
            self.assertIn('FOR UPDATE',source)
            self.assertNotIn('DROP TABLE',source)
            self.assertIn('DROP FUNCTION',source)


if __name__=='__main__':unittest.main()
