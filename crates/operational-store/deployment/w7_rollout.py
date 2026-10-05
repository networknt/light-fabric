#!/usr/bin/env python3
"""Reviewed owner-run E04 deployment switch tools. Tests inject an in-memory adapter."""
import argparse
from contextlib import contextmanager
from datetime import datetime, timezone
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import uuid
from w7_ownership import OwnershipContract, OwnershipError, verify_companion
from bundle_contract import CURRENT_VERSION, verify_bundle

PROFILE = 'cel-workflow-v2'
IDENT = re.compile(r'[a-z][a-z0-9_]{0,62}\Z')
NAME = re.compile(r'[A-Za-z0-9_.-]{1,128}\Z')
DIGEST = re.compile(r'sha256:[0-9a-f]{64}\Z')
ROLES = {'workflow', 'portal-admission', 'portal-projection', 'controller',
         'gateway', 'agent', 'runner', 'infrastructure'}
REQUIRED = {
    'portal-admission': {'independent-admission', 'validator-build-allowlist', 'receipt-recovery'},
    'portal-projection': {'historical-replay-no-fresh-intent', 'durable-evidence-retention'},
    'controller': {'execution-results-page-v1'},
    'gateway': {'workflow-validation-route-acl'},
    'agent': {'prepared-payload-results'}, 'runner': {'prepared-payload-results'},
    'infrastructure': set(), 'workflow': set(),
}


class Refusal(Exception):
    """Only fixed error codes are printed, never external messages or values."""


def require(condition, code):
    if not condition:
        raise Refusal(code)


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def timestamp(value):
    try:
        parsed = datetime.fromisoformat(value.replace('Z', '+00:00'))
        require(parsed.tzinfo is not None, 'TIME_INVALID')
        return parsed
    except (TypeError, ValueError):
        raise Refusal('TIME_INVALID') from None


def fresh(value, now, seconds=90):
    age = (now - timestamp(value)).total_seconds()
    require(0 <= age <= seconds, 'EVIDENCE_STALE_OR_FUTURE')


def load_json(path):
    try:
        require(Path(path).stat().st_size <= 2 * 1024 * 1024, 'INPUT_TOO_LARGE')
        return json.loads(Path(path).read_text())
    except (OSError, ValueError):
        raise Refusal('INPUT_UNAVAILABLE_OR_INVALID') from None


def atomic_json(path, data):
    path = Path(path)
    temporary = path.with_name(path.name + '.' + uuid.uuid4().hex + '.tmp')
    fd = os.open(temporary, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    with os.fdopen(fd, 'w') as stream:
        json.dump(data, stream, sort_keys=True)
        stream.write('\n')
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, path)
    fd = os.open(path.parent, os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


@contextmanager
def serialized(state_dir):
    state_dir = Path(state_dir)
    state_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    with (state_dir / 'w7.lock').open('a') as stream:
        try:
            fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise Refusal('OPERATION_BUSY') from None
        yield


def validate_plan(plan):
    require(plan.get('version') == 1, 'PLAN_VERSION')
    require(type(plan.get('freshness_seconds', 90)) is int and
            0 < plan.get('freshness_seconds', 90) <= 90, 'FRESHNESS_BOUND')
    require(NAME.fullmatch(plan.get('deployment_id', '')) is not None, 'PLAN_ID')
    gates = plan.get('gates', [])
    require(gates and any(g['kind'] == 'portal' for g in gates)
            and any(g['kind'] == 'operational' for g in gates), 'BOTH_DATABASE_LANES_REQUIRED')
    require(len({g['id'] for g in gates}) == len(gates), 'DUPLICATE_GATE')
    require(len({g['pgservice'] for g in gates}) == len(gates) and
            len({g['database'] for g in gates}) == len(gates), 'AMBIGUOUS_GATE_TARGET')
    for gate in gates:
        require(gate['kind'] in {'portal', 'operational'}, 'GATE_KIND')
        require(NAME.fullmatch(gate['id']) and NAME.fullmatch(gate['pgservice']), 'GATE_ID')
        require(IDENT.fullmatch(gate['database']), 'DATABASE_ID')
        require(gate['schema'] == ('public' if gate['kind'] == 'portal' else 'workflow_ops'), 'SCHEMA_ID')
        if gate['kind'] == 'operational':
            uuid.UUID(gate['scope_root_id'])
    placements = plan.get('placements', [])
    require(placements and len({p['id'] for p in placements}) == len(placements), 'PLACEMENTS_REQUIRED')
    for placement in placements:
        require(NAME.fullmatch(placement['id']), 'PLACEMENT_ID')
        require(placement['adapter'] in {'docker-compose', 'owner-orchestrator-snapshot'}, 'PLACEMENT_ADAPTER')
        if placement['adapter'] == 'docker-compose':
            require(NAME.fullmatch(placement.get('project', '')), 'PROJECT_ID')
        require(placement['gate_ids'] and set(placement['gate_ids']) <= {g['id'] for g in gates}, 'PLACEMENT_GATES')
        require(placement['services'], 'SERVICE_INVENTORY_REQUIRED')
        for service, spec in placement['services'].items():
            require(NAME.fullmatch(service) and spec['role'] in ROLES, 'UNCLASSIFIED_SERVICE')
            require(spec['minimum_instances'] >= 0 and spec['maximum_instances'] >= spec['minimum_instances'], 'REPLICA_RANGE')
            require(spec['images'] and all(DIGEST.fullmatch(x) for x in spec['images']), 'IMMUTABLE_IMAGES_REQUIRED')
            require(all(DIGEST.fullmatch(x) for x in spec.get('stopped_images', [])), 'IMMUTABLE_IMAGES_REQUIRED')
            if spec['role'] == 'workflow':
                require(spec.get('gate_id') in placement['gate_ids'] and
                        next(g for g in gates if g['id'] == spec['gate_id'])['kind'] == 'operational', 'WORKFLOW_GATE_SCOPE')
                require(spec.get('supported_profiles') == ['cel-workflow-v1', PROFILE]
                        and spec.get('admits_profiles') == ['cel-workflow-v1', PROFILE]
                        and spec.get('binary_version'), 'WORKFLOW_EXPECTATION_REQUIRED')
    require({g['id'] for g in gates} <= set().union(*(set(p['gate_ids']) for p in placements)), 'UNCOVERED_GATE')


def verify_assets(root):
    root = Path(root)
    verify_companion(root)
    require(sha(root / 'w7-assets.json') == 'cce8a4be907461da98dba854262eb2565772ac3f3722d471e78dfaa4679b4650',
            'ACCEPTED_ASSET_DESCRIPTOR_IDENTITY')
    pins = load_json(root / 'w7-assets.json')
    require(pins['files'].get('bundle/bundle.sha256') == '461c90e5c46acb31550c6ecc16786a397625655ef3ad62d45c7e0be6844ac2af', 'ACCEPTED_BUNDLE_IDENTITY')
    require(pins['files'].get('portal/patch_20261002_01_workflow_expression_profile.sql') == '96e2cbce858800e137f8718ce907abe0350a482452d0016e3c4cf9883edc009f'
            and pins['files'].get('portal/patch_20261002_02_workflow_delivery_ledger.sql') == '726eb3549e85be0f31deb21163a6d2addb0dd4fe34d5dfc85be9c5060540f4a3', 'ACCEPTED_PORTAL_IDENTITY')
    for relative, expected in pins['files'].items():
        path = root / relative
        require(path.resolve().is_relative_to(root.resolve()), 'ASSET_PATH')
        require(path.is_file() and sha(path) == expected, 'ASSET_DIGEST_MISMATCH')
    bundle = root / 'bundle'
    manifest = load_json(bundle / 'manifest.json')
    try:
        version, verified_rows = verify_bundle(bundle)
    except (OSError, ValueError, KeyError):
        raise Refusal('BUNDLE_CONTRACT') from None
    require(version == CURRENT_VERSION, 'BUNDLE_CONTRACT')
    listed = {}
    for line in (bundle / 'bundle.sha256').read_text().splitlines():
        expected, relative = line.split('  ', 1)
        require(relative not in listed, 'DUPLICATE_BUNDLE_FILE')
        listed[relative] = expected
        require(pins['files'].get('bundle/' + relative) == expected, 'BUNDLE_COVERAGE')
    actual = {str(p.relative_to(bundle)) for p in bundle.rglob('*') if p.is_file()}
    require(actual == set(listed) | {'bundle.sha256'}, 'BUNDLE_FILE_SET')
    rows = []
    for line in (bundle / 'migration-order.tsv').read_text().splitlines():
        if not line or line.startswith('#'):
            continue
        order, owner, schema, migration, relative, expected = line.split('\t')
        rows.append((int(order), owner, schema, migration, relative, expected))
    require(rows == verified_rows, 'MIGRATION_ORDER')
    for row, entry in zip(rows, manifest['orderedMigrations']):
        require(row == (entry['order'], entry['owner'], entry['schema'], entry['migrationId'], entry['path'], entry['sha256']), 'MANIFEST_ORDER')
        require(listed.get(row[4]) == row[5] and not row[4].startswith('rollback/'), 'FORWARD_DIGEST')
    return pins, rows


def preflight_sql(root, gate):
    relative = ('portal/patch_20261002_01_workflow_expression_profile.sql' if gate['kind'] == 'portal'
                else 'bundle/crates/workflow-store/migrations/workflow-postgres/0024_workflow_expression_profile.sql')
    text = (Path(root) / relative).read_text()
    end = text.index('$preflight$;', text.index('DO $preflight$')) + len('$preflight$;')
    # Exact accepted prefix: no parser replacement, relaxed match or stored-data mutation.
    sql = text[:end] + '\nCOMMIT;\n'
    expected = ('e90cd3973cab9c8274daed6baa41d6aa168049dafa17dd21659e8c981cf816a8' if gate['kind'] == 'portal'
                else '750b8408d2f2883565a7978303e1e1eb7443993b9aba43f14b05de115ea124ac')
    require(hashlib.sha256(sql.encode()).hexdigest() == expected, 'PREFLIGHT_BYTES_CHANGED')
    return sql


def inventory(plan, evidence, backend, now, phase):
    require(evidence.get('version') == 1 and evidence.get('deployment_id') == plan['deployment_id'], 'EVIDENCE_SCOPE')
    require(evidence.get('plan_sha256') == backend.plan_digest, 'EVIDENCE_PLAN')
    fresh(evidence['captured_at'], now)
    require(evidence.get('owner_review') and evidence.get('quiescence_operation_id'), 'QUIESCENCE_REQUIRED')
    proof = {p['id']: p for p in evidence['placements']}
    require(len(proof) == len(evidence['placements']) and set(proof) == {p['id'] for p in plan['placements']}, 'PLACEMENT_COVERAGE')
    fingerprints, workflow_ids = [], set()
    for placement in plan['placements']:
        recorded = proof[placement['id']]
        fresh(recorded['captured_at'], now)
        require(recorded.get('all_instances_accounted_for') is True and recorded.get('external_writers_quiesced') is True
                and recorded.get('activation_operators_excluded') is True and recorded.get('authority_reference'), 'QUIESCENCE_UNPROVEN')
        instances = backend.instances(placement, recorded)
        counts = {s: 0 for s in placement['services']}
        ids = set()
        for instance in instances:
            require(instance['id'] not in ids, 'DUPLICATE_INSTANCE')
            ids.add(instance['id'])
            require(instance['service'] in counts, 'UNCLASSIFIED_INSTANCE')
            spec = placement['services'][instance['service']]
            require(instance['state'] in {'running', 'stopped', 'completed'}, 'INSTANCE_STATE')
            fingerprints.append((placement['id'], instance['id'], instance['service'],
                                 instance['image_id'], instance['state'], instance['started_at']))
            # Stopped instances are explicitly counted as non-running evidence;
            # every running instance, including infrastructure, must be classified.
            if instance['state'] != 'running':
                require(instance['image_id'] in spec['images'] + spec.get('stopped_images', []), 'UNKNOWN_IMAGE')
                continue
            require(instance['image_id'] in spec['images'], 'UNKNOWN_IMAGE')
            counts[instance['service']] += 1
            require(timestamp(instance['started_at']) <= now, 'INSTANCE_START_IN_FUTURE')
            readiness = recorded.get('readiness', {}).get(instance['id'], {})
            if spec['role'] != 'workflow':
                require(readiness.get('image_id') == instance['image_id'] and readiness.get('started_at') == instance['started_at'], 'ROLE_READINESS_MISSING')
                fresh(readiness['checked_at'], now)
                require(REQUIRED[spec['role']] <= set(readiness.get('contracts', [])), 'ROLE_CONTRACT_MISSING')
                if spec['role'] == 'controller':
                    require(readiness.get('authenticated_page_probe') == {'status': 200, 'envelope': 'items,nextCursor', 'route': '/internal/execution/results/page'}, 'CONTROLLER_PAGE_MISSING')
            else:
                identity = backend.workflow_identity(placement, instance, recorded)
                require(identity not in workflow_ids, 'AMBIGUOUS_HEARTBEAT_IDENTITY')
                workflow_ids.add(identity)
                gate_id = spec['gate_id']
                require(gate_id in placement['gate_ids'], 'WORKFLOW_GATE_SCOPE')
                row = backend.heartbeat(gate_id, identity)
                require(row['instance_id'] == identity and row['binary_version'] == spec['binary_version'], 'HEARTBEAT_MISMATCH')
                require(sorted(row['supported_profiles']) == sorted(spec['supported_profiles'])
                        and sorted(row['admits_profiles']) == sorted(spec['admits_profiles']), 'CAPABILITY_MISMATCH')
                require(0 <= row['age_seconds'] <= plan.get('freshness_seconds', 90), 'HEARTBEAT_STALE_OR_FUTURE')
                require(timestamp(row['heartbeat_ts']) >= timestamp(instance['started_at']), 'HEARTBEAT_PREVIOUS_PROCESS')
        for service, count in counts.items():
            spec = placement['services'][service]
            minimum = (0 if phase in {'prepare', 'restart'} and spec['role'] != 'controller'
                       else spec['minimum_instances'])
            require(minimum <= count <= spec['maximum_instances'], 'INCOMPLETE_INSTANCE_INVENTORY')
    return sorted(fingerprints)


def docker_instance(raw):
    """Pure identity-only parser; paused/restarting is not stopped evidence."""
    parts = re.findall(r'"[^"\n]*"|null|true|false', raw)
    require(len(parts) == 7, 'ORCHESTRATOR_INSTANCE_INVALID')
    iid, image, state, started, service, paused, restarting = [json.loads(x) for x in parts]
    require(state in {'running', 'exited', 'created'} and paused is False and restarting is False,
            'ORCHESTRATOR_STATE_AMBIGUOUS')
    return {'id': iid, 'image_id': image, 'state': 'running' if state == 'running' else 'stopped',
            'started_at': started, 'service': service}


def migration_needed(recorded, digest, kind, migration):
    """Pure ledger decision: exact applied bytes skip; missing baseline refuses."""
    if recorded:
        require(recorded == digest, 'LEDGER_DIGEST_MISMATCH')
        return False
    if kind == 'operational':
        require(migration in {'0024_workflow_expression_profile', '0025_workflow_operation_receipts', '0026_host_tool_workflow_access'},
                'BASELINE_LEDGER_REQUIRED')
    return True


class Coordinator:
    def __init__(self, plan, backend, root, state_dir, evidence):
        validate_plan(plan)
        self.plan, self.backend, self.root = plan, backend, Path(root)
        self.state_dir, self.evidence = Path(state_dir), evidence
        self.gates = plan['gates']
        self.portal = [g for g in self.gates if g['kind'] == 'portal']
        self.operational = [g for g in self.gates if g['kind'] == 'operational']
        self.record = {'deployment_id': plan['deployment_id'], 'plan_sha256': backend.plan_digest,
                       'operation_id': uuid.uuid4().hex, 'events': []}

    def event(self, code, gate=None):
        self.record['events'].append({'code': code, 'gate': None if gate is None else gate['id']})
        atomic_json(self.state_dir / 'operation.json', self.record)

    def states(self):
        states = {}
        for gate in self.gates:
            try:
                value = self.backend.state(gate)
                states[gate['id']] = value if value in {'ON', 'OFF', 'ABSENT'} else 'UNKNOWN'
            except Exception:
                states[gate['id']] = 'UNKNOWN'
        return states

    def all_off(self):
        states = self.states()
        require(all(s == 'OFF' for s in states.values()), 'ADMISSION_NOT_VERIFIED_OFF')

    def shutdown(self):
        # Independent commits; continue disabling reachable targets even when an
        # earlier database is unreachable. Never infer OFF from transaction failure.
        failures = []
        for gate in self.portal + self.operational:
            self.event('OFF_INTENT', gate)
            try:
                self.backend.toggle(gate, False)
                require(self.backend.state(gate) == 'OFF', 'OFF_READBACK_FAILED')
                self.event('OFF_VERIFIED', gate)
            except Exception:
                failures.append(gate['id'])
                self.event('OFF_UNKNOWN_OR_FAILED', gate)
        require(not failures, 'PARTIAL_SHUTDOWN')
        self.all_off()

    def readiness(self, phase):
        return inventory(self.plan, self.evidence, self.backend, self.backend.now(), phase)

    def prepare(self):
        states = self.states()
        require(all(v in {'OFF', 'ABSENT'} for v in states.values()), 'PREPARE_ALREADY_ON_OR_UNKNOWN')
        self.backend.coverage(self.plan, self.evidence)
        self.event('PREFLIGHT_1')
        for gate in self.gates:
            self.backend.preflight(gate, preflight_sql(self.root, gate))
        self.event('VERIFY_DIGESTS')
        verify_assets(self.root)
        # Validate EVERY gate before the first install transaction. A later
        # gate's missing qualification, wrong ledger or owner cannot leave an
        # earlier gate migrated. Repeat per-gate checks before each transaction.
        for gate in self.gates:
            self.backend.preinstall(gate)
        self.event('ALL_GATES_PREINSTALL_VERIFIED')
        # Removing a previously ready marker is deliberate before any install;
        # interrupted preparation cannot leave restart permission behind.
        (self.state_dir / 'startup-ready.sha256').unlink(missing_ok=True)
        for gate in self.gates:
            self.event('SCHEMA_INTENT', gate)
            self.backend.install(gate)
            require(self.backend.state(gate) == 'OFF', 'PREPARATION_NOT_OFF')
            self.backend.schema(gate, claim_required=True)
            self.event('SCHEMA_VERIFIED_OFF', gate)
        self.all_off()
        ready = {'plan_sha256': self.backend.plan_digest, 'deployment_id': self.plan['deployment_id'],
                 'gates': [g['id'] for g in self.gates], 'operation_id': self.record['operation_id']}
        atomic_json(self.state_dir / 'prepared.json', ready)
        self.backend.write_startup_marker(self.state_dir)
        self.event('PREPARED')

    def ownership_check(self):
        verify_assets(self.root)
        for gate in self.gates:
            self.backend.preinstall(gate, transition=True)
        self.event('ALL_GATES_OWNERSHIP_CHECKED')

    def ownership_transition(self):
        verify_assets(self.root)
        self.backend.coverage(self.plan, self.evidence)
        # Preflight and qualification for Portal and ALL operational gates
        # precede the first explicit administrative owner change.
        for gate in self.gates:
            self.backend.preflight(gate, preflight_sql(self.root, gate))
        for gate in self.gates:
            self.backend.preinstall(gate, transition=True)
        for gate in self.operational:
            before = self.backend.preinstall(gate, transition=True)
            audit_path = self.state_dir / ('ownership-' + self.record['operation_id'] + '-' + gate['id'] + '.json')
            audit = {'operation_id': self.record['operation_id'], 'gate': gate['id'],
                     'plan_sha256': self.backend.plan_digest,
                     'companion_sha256': verify_companion(self.root),
                     'database': gate['database'], 'scope_root_id': gate['scope_root_id'],
                     'sql_assets_sha256': sha(self.root / 'w7-assets.json'),
                     'expected_after_sha256': before['qualification']['preinstall'].get('0:migrator'),
                     'server': self.backend.sql(gate, 'SELECT json_build_object(\'address\',inet_server_addr(),\'port\',inet_server_port(),\'version\',current_setting(\'server_version\'));'),
                     'actor': self.backend.sql(gate, 'SELECT session_user;'),
                     'before': before['snapshot'], 'status': 'INTENT'}
            atomic_json(audit_path, audit)
            if before['owner_class'] == 'migrator':
                audit['status'] = 'INTENDED_BASELINE_NOOP'
                prior = []
                for path in sorted(self.state_dir.glob('ownership-*-'+gate['id']+'.json')):
                    if path == audit_path:
                        continue
                    item = load_json(path)
                    if (item.get('plan_sha256') == self.backend.plan_digest
                            and item.get('companion_sha256') == audit['companion_sha256']
                            and item.get('status') in {'INTENT', 'RESPONSE_LOST_OR_FAILED'}):
                        prior.append({'path': path.name, 'sha256': sha(path)})
                if prior:
                    audit['status'] = 'READBACK_RECOVERY_OBSERVED_CONVERGENCE'
                    audit['prior_uncertain_attempts'] = prior
            else:
                try:
                    self.backend.sql(gate, OwnershipContract(self.backend).transition_sql(gate, before))
                except Exception:
                    # Preserve uncertainty. A later readback can establish
                    # convergence, but cannot manufacture commit acknowledgement.
                    audit['status'] = 'RESPONSE_LOST_OR_FAILED'
                    atomic_json(audit_path, audit)
                    raise
                audit['status'] = 'COMMITTED_AND_READ_BACK'
            after = self.backend.preinstall(gate)
            audit['after'] = after['snapshot']
            atomic_json(audit_path, audit)
            self.event('OWNERSHIP_VERIFIED', gate)

    def activate(self):
        states = self.states()
        require(all(v == 'OFF' for v in states.values()), 'ACTIVATE_ALREADY_ON_OR_RECOVERY_REQUIRED')
        prepared = load_json(self.state_dir / 'prepared.json')
        require(prepared['plan_sha256'] == self.backend.plan_digest, 'PREPARATION_IDENTITY')
        verify_assets(self.root)
        first = self.readiness('activate')
        for gate in self.gates:
            self.backend.schema(gate, claim_required=True)
        self.event('INVENTORY_VERIFIED')
        self.event('PREFLIGHT_2')
        for gate in self.gates:
            self.backend.preflight(gate, preflight_sql(self.root, gate))
        require(first == self.readiness('activate'), 'INVENTORY_CHANGED')
        self.all_off()
        try:
            for gate in self.operational + self.portal:
                self.event('ON_INTENT', gate)
                self.backend.toggle(gate, True)
                require(self.backend.state(gate) == 'ON', 'ON_READBACK_FAILED')
                self.event('ON_VERIFIED', gate)
        except BaseException:
            # Best effort, not a global-OFF assertion; SIGKILL cannot run this.
            try:
                self.shutdown()
            except Exception:
                pass
            raise
        self.event('ACTIVATED')

    def rollback(self):
        verify_assets(self.root)
        first = self.readiness('rollback')
        for gate in self.gates:
            self.backend.schema(gate, claim_required=self.backend.claim_present(gate))
        self.event('ROLLBACK_INVENTORY_VERIFIED')
        (self.state_dir / 'startup-ready.sha256').unlink(missing_ok=True)
        self.shutdown()
        for gate in self.gates:
            self.backend.active_run_check(gate)
        require(first == self.readiness('rollback'), 'INVENTORY_CHANGED')
        for gate in self.portal + self.operational:
            receipt = self.state_dir / ('rollback-' + gate['id'] + '.json')
            claim = self.backend.claim_present(gate)
            if not claim:
                previous = load_json(receipt)
                require(previous['plan_sha256'] == self.backend.plan_digest
                        and previous['success'] is True, 'ROLLBACK_PROVENANCE_MISSING')
            else:
                self.event('ROLLBACK_SQL_INTENT', gate)
                self.backend.rollback(gate)
            self.backend.active_run_check(gate)
            self.backend.schema(gate, claim_required=False)
            require(self.backend.state(gate) == 'OFF' and not self.backend.claim_present(gate), 'ROLLBACK_POSTCHECK')
            atomic_json(receipt, {'plan_sha256': self.backend.plan_digest, 'success': True,
                                 'operation_id': self.record['operation_id']})
            self.event('ROLLBACK_VERIFIED', gate)
        self.all_off()
        require(first == self.readiness('rollback'), 'INVENTORY_CHANGED')
        self.event('DOWNGRADE_ELIGIBLE_REVERIFY_BEFORE_OWNER_ACTION')
        # No binary downgrade, local permit with unlimited lifetime, or cleanup.

    def recover_off(self):
        self.backend.coverage(self.plan, self.evidence)
        self.shutdown()
        self.event('RECOVERY_VERIFIED_OFF')

    def restart_check(self):
        verify_assets(self.root)
        prepared = load_json(self.state_dir / 'prepared.json')
        require(prepared['plan_sha256'] == self.backend.plan_digest, 'PREPARATION_IDENTITY')
        require((self.state_dir / 'startup-ready.sha256').is_file(), 'STARTUP_NOT_PREPARED')
        for gate in self.gates:
            self.backend.schema(gate, claim_required=True)
        # Existing ON is permitted for a restart, never a preparation replay.
        require(all(v in {'ON', 'OFF'} for v in self.states().values()), 'GATE_STATE_UNKNOWN')
        self.readiness('restart')
        self.event('CONTROLLER_READY_BEFORE_WORKFLOW_START')


class LiveBackend:
    """Owner-only adapters. No mock test instantiates or invokes this class."""
    def __init__(self, plan, plan_digest, root):
        self.plan, self.plan_digest, self.root = plan, plan_digest, Path(root)
        self.by_id = {g['id']: g for g in plan['gates']}
        # Input parsing only. Full bundle verification occurs after preflight #1
        # in prepare, before any schema installation.
        self.pins = load_json(self.root / 'w7-assets.json')
        entries = load_json(self.root / 'bundle/manifest.json')['orderedMigrations']
        self.rows = [(e['order'],e['owner'],e['schema'],e['migrationId'],e['path'],e['sha256']) for e in entries]

    @staticmethod
    def now():
        return datetime.now(timezone.utc)

    @staticmethod
    def command(argv, data=None, include_stderr=False):
        try:
            result = subprocess.run(argv, input=data, text=True, capture_output=True, timeout=120,
                                    env={**os.environ, 'PGCONNECT_TIMEOUT': '10', 'PGOPTIONS': '-c statement_timeout=90000 -c lock_timeout=10000'})
            require(result.returncode == 0, 'ADAPTER_FAILED')
            output = result.stdout + (result.stderr if include_stderr else '')
            require(len(output) <= 2 * 1024 * 1024, 'ADAPTER_OUTPUT_LIMIT')
            return output
        except (OSError, subprocess.TimeoutExpired):
            raise Refusal('ADAPTER_UNAVAILABLE') from None

    def sql(self, gate, text):
        return self.command(['psql', '-X', '-qAt', '-v', 'ON_ERROR_STOP=1',
                             'service=' + gate['pgservice']], text).strip()

    def identity(self, gate):
        require(self.sql(gate, 'SELECT current_database();') == gate['database'], 'DATABASE_IDENTITY')
        if gate['kind'] == 'operational':
            scope = self.sql(gate, 'SELECT scope_root_id::text FROM operational_meta.operational_database_identity_t;')
            require(scope == gate['scope_root_id'], 'DATABASE_SCOPE')

    def state(self, gate):
        self.identity(gate)
        schema = gate['schema']
        exists = self.sql(gate, f"SELECT to_regclass('{schema}.workflow_expression_profile_policy_t') IS NOT NULL;")
        if exists == 'f':
            return 'ABSENT'
        raw = self.sql(gate, f"SELECT CASE WHEN admission_enabled THEN 'ON' ELSE 'OFF' END FROM {schema}.workflow_expression_profile_policy_t WHERE profile_id='{PROFILE}';")
        return raw if raw in {'ON', 'OFF'} else 'UNKNOWN'

    def toggle(self, gate, enabled):
        self.identity(gate)
        s, value = gate['schema'], 'true' if enabled else 'false'
        self.sql(gate, f"""BEGIN;
DO $gate$ BEGIN
 PERFORM 1 FROM {s}.workflow_expression_profile_policy_t WHERE profile_id='{PROFILE}' FOR UPDATE;
 IF NOT FOUND THEN RAISE EXCEPTION 'POLICY_MISSING'; END IF;
 UPDATE {s}.workflow_expression_profile_policy_t SET admission_enabled={value},
 updated_ts=clock_timestamp(), updated_by=SESSION_USER WHERE profile_id='{PROFILE}';
END $gate$;
COMMIT;""")

    def preflight(self, gate, sql):
        self.identity(gate)
        self.sql(gate, sql)

    def migrations(self, gate):
        if gate['kind'] == 'operational':
            return [(migration, self.root / 'bundle' / path, digest, owner, schema)
                    for _, owner, schema, migration, path, digest in self.rows]
        return [(Path(path).stem, self.root / path, digest, 'portal', 'public')
                for path, digest in self.pins['files'].items()
                if path.startswith('portal/patch_')]

    def ledger(self, gate, migration, owner, schema):
        if gate['kind'] == 'operational':
            return self.sql(gate, "SELECT migration_digest FROM operational_meta.operational_schema_migration_t "
                f"WHERE migration_owner='{owner}' AND schema_name='{schema}' AND migration_id='{migration}';").removeprefix('sha256:')
        # Existing reviewed Portal patch tracking convention; this tool never
        # invents a schema table or regenerates the inventory.
        return self.sql(gate, f"SELECT checksum FROM public.portal_schema_patch_t WHERE patch_id='{migration}';")

    def install(self, gate):
        self.identity(gate)
        require(self.state(gate) in {'OFF', 'ABSENT'}, 'PREPARATION_NOT_OFF')
        self.preinstall(gate)
        for migration, path, digest, owner, schema in self.migrations(gate):
            recorded = self.ledger(gate, migration, owner, schema)
            if not migration_needed(recorded, digest, gate['kind'], migration):
                continue
            self.preinstall(gate)
            # Baseline setup is an owner qualification prerequisite, not an
            # implicit role/database provisioning operation in rollout-prepare.
            text = path.read_text()
            body = '\n'.join(line for line in text.splitlines() if line not in {'BEGIN;', 'COMMIT;'})
            if gate['kind'] == 'operational':
                database = gate['database']
                body = body.replace('operations_', database + '_').replace('ON DATABASE operations', 'ON DATABASE ' + database).replace('IN DATABASE operations', 'IN DATABASE ' + database).replace("database_identity = 'operations'", "database_identity = '" + database + "'")
                ledger = f"INSERT INTO operational_meta.operational_schema_migration_t(migration_owner,schema_name,migration_id,migration_digest,bundle_version,contract_generation) VALUES('{owner}','{schema}','{migration}','sha256:{digest}','{CURRENT_VERSION}',2);"
                role = database + '_workflow_migrator'
                body = f'SET LOCAL ROLE {role};\n' + body + '\nRESET ROLE;'
            else:
                ledger = f"INSERT INTO public.portal_schema_patch_t(patch_id,checksum) VALUES('{migration}','{digest}');"
            self.sql(gate, 'BEGIN;\n' + body + '\n' + ledger + '\nCOMMIT;')
            require(self.ledger(gate, migration, owner, schema) == digest, 'MIGRATION_READBACK_FAILED')
            self.preinstall(gate)

    def preinstall(self, gate, transition=False):
        return OwnershipContract(self).validate(gate, transition)

    def claim_present(self, gate):
        return self.sql(gate, f"SELECT to_regprocedure('{gate['schema']}.workflow_claim_host_task_v2(uuid,integer,text[])') IS NOT NULL;") == 't'

    def schema(self, gate, claim_required):
        self.identity(gate)
        OwnershipContract(self).reference(gate)
        s = gate['schema']
        for migration, _, digest, owner, schema in self.migrations(gate):
            require(self.ledger(gate, migration, owner, schema) == digest, 'SCHEMA_LEDGER_MISMATCH')
        names = ['process_info_t', 'workflow_expression_profile_policy_t', 'workflow_worker_capability_t']
        names += ['workflow_operation_receipt_t'] if gate['kind'] == 'operational' else ['workflow_delivery_intent_t', 'workflow_command_receipt_t']
        for name in names:
            require(self.sql(gate, f"SELECT to_regclass('{s}.{name}') IS NOT NULL;") == 't', 'REQUIRED_SCHEMA_MISSING')
        ck = self.sql(gate, f"SELECT count(*) FROM pg_constraint WHERE conrelid='{s}.process_info_t'::regclass AND conname='process_expression_profile_snapshot_ck' AND convalidated;")
        require(ck == '1', 'PROFILE_CHECK_MISSING')
        v1 = self.sql(gate, f"SELECT pg_get_functiondef('{s}.workflow_claim_host_task_v1(uuid,integer)'::regprocedure);")
        require("expression_profile='cel-workflow-v1'" in v1, 'LEGACY_CLAIM_UNPROTECTED')
        if claim_required:
            require(self.claim_present(gate), 'REACTIVATION_REQUIRES_REVIEWED_RESTORATION')
        if gate['kind'] == 'operational':
            role = gate['database'] + '_workflow_runtime'
            privileges = self.sql(gate, f"SELECT has_table_privilege('{role}','{s}.workflow_expression_profile_policy_t','SELECT') AND has_column_privilege('{role}','{s}.workflow_expression_profile_policy_t','profile_id','UPDATE') AND NOT has_column_privilege('{role}','{s}.workflow_expression_profile_policy_t','admission_enabled','UPDATE') AND NOT has_table_privilege('{role}','{s}.workflow_expression_profile_policy_t','INSERT') AND NOT has_table_privilege('{role}','{s}.workflow_expression_profile_policy_t','DELETE');")
            require(privileges == 't', 'POLICY_PRIVILEGES_INVALID')
        expected = gate.get('catalog_fingerprint' if claim_required else 'rollback_catalog_fingerprint', '')
        require(DIGEST.fullmatch(expected) and gate.get('qualification_reference'), 'DATABASE_QUALIFICATION_EVIDENCE_REQUIRED')
        require(self.catalog_fingerprint(gate, claim_required) == expected, 'QUALIFIED_CATALOG_MISMATCH')

    def catalog_fingerprint(self, gate, include_claim=True):
        s = gate['schema']
        query = f"""SELECT json_build_object(
 'functions',(SELECT coalesce(json_agg(x ORDER BY x.name),'[]'::json) FROM
 (SELECT p.proname name,pg_get_functiondef(p.oid) definition,p.proacl::text acl,pg_get_userbyid(p.proowner) owner FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace
 WHERE n.nspname='{s}' AND (p.proname IN ('workflow_claim_host_task_v1','workflow_receipt_immutable','workflow_durable_evidence_guard')
 {'OR p.proname=\'workflow_claim_host_task_v2\'' if include_claim else ''})) x),
 'constraints',(SELECT coalesce(json_agg(x ORDER BY x.name),'[]'::json) FROM
 (SELECT conname name,pg_get_constraintdef(oid) definition,convalidated FROM pg_constraint
 WHERE connamespace='{s}'::regnamespace AND (conname='process_expression_profile_snapshot_ck'
 OR conrelid IN (SELECT oid FROM pg_class WHERE relnamespace='{s}'::regnamespace AND relname LIKE 'workflow_%receipt%'))) x),
 'tables',(SELECT coalesce(json_agg(x ORDER BY x.name),'[]'::json) FROM
 (SELECT relname name,relacl::text acl,pg_get_userbyid(relowner) owner FROM pg_class WHERE relnamespace='{s}'::regnamespace
 AND relname IN ('workflow_expression_profile_policy_t','workflow_worker_capability_t','workflow_operation_receipt_t',
 'workflow_command_receipt_t','workflow_delivery_intent_t','workflow_start_request_t','workflow_sync_target_t','process_expression_profile_claim_idx')) x),
 'triggers',(SELECT coalesce(json_agg(x ORDER BY x.name),'[]'::json) FROM
 (SELECT tgname name,pg_get_triggerdef(t.oid) definition,tgenabled FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid
 WHERE c.relnamespace='{s}'::regnamespace AND c.relname IN ('workflow_operation_receipt_t','workflow_command_receipt_t',
 'workflow_delivery_intent_t','workflow_start_request_t') AND NOT t.tgisinternal) x),
 'indexes',(SELECT coalesce(json_agg(x ORDER BY x.name),'[]'::json) FROM
 (SELECT c.relname name,pg_get_indexdef(i.indexrelid) definition,i.indisvalid,i.indisready FROM pg_index i
 JOIN pg_class c ON c.oid=i.indexrelid JOIN pg_class t ON t.oid=i.indrelid
 WHERE t.relnamespace='{s}'::regnamespace AND (t.relname LIKE 'workflow_%' OR
 c.relname='process_expression_profile_claim_idx')) x),
 'columns',(SELECT coalesce(json_agg(x ORDER BY x.table_name,x.name),'[]'::json) FROM
 (SELECT c.relname table_name,a.attname name,format_type(a.atttypid,a.atttypmod) type,a.attnotnull,a.attacl::text acl
 FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid WHERE c.relnamespace='{s}'::regnamespace AND a.attnum>0 AND NOT a.attisdropped
 AND (c.relname IN ('workflow_expression_profile_policy_t','workflow_worker_capability_t','workflow_operation_receipt_t',
 'workflow_command_receipt_t','workflow_delivery_intent_t','workflow_start_request_t','workflow_sync_target_t') OR
 (c.relname='process_info_t' AND a.attname='expression_profile'))) x));"""
        parsed = json.loads(self.sql(gate, query))
        return 'sha256:' + hashlib.sha256(json.dumps(parsed,sort_keys=True,separators=(',',':')).encode()).hexdigest()

    def active_run_check(self, gate):
        s = gate['schema']
        self.sql(gate, f"""BEGIN;
DO $check$ DECLARE enabled boolean; BEGIN
 SELECT admission_enabled INTO enabled FROM {s}.workflow_expression_profile_policy_t WHERE profile_id='{PROFILE}' FOR UPDATE;
 IF NOT FOUND OR enabled IS DISTINCT FROM false THEN RAISE EXCEPTION 'ADMISSION_NOT_OFF'; END IF;
 IF EXISTS(SELECT 1 FROM {s}.process_info_t WHERE expression_profile<>'cel-workflow-v1'
 AND (status_code NOT IN ('C','F') OR completed_ts IS NULL)) THEN RAISE EXCEPTION 'ACTIVE_NONLEGACY'; END IF;
END $check$;
COMMIT;""")

    def rollback(self, gate):
        # Each accepted file is standalone; retain its independent OFF commit.
        files = (['portal/rollback/rollback_20261002_02_workflow_delivery_ledger.sql',
                  'portal/rollback/rollback_20261002_01_workflow_expression_profile.sql'] if gate['kind'] == 'portal' else
                 ['bundle/rollback/0025_workflow_operation_receipts.sql', 'bundle/rollback/0024_workflow_expression_profile.sql'])
        for relative in files:
            self.sql(gate, (self.root / relative).read_text())

    def coverage(self, plan, evidence):
        # Prepare/recovery need quiescence completeness, not already-upgraded
        # image/capability evidence. Those are separately required for activation.
        require(evidence.get('plan_sha256') == self.plan_digest
                and evidence.get('deployment_id') == plan['deployment_id']
                and evidence.get('owner_review') and evidence.get('quiescence_operation_id'), 'QUIESCENCE_REQUIRED')
        fresh(evidence['captured_at'], self.now())
        proofs = {p['id']: p for p in evidence['placements']}
        require(len(proofs) == len(evidence['placements']) and set(proofs) == {p['id'] for p in plan['placements']}, 'PLACEMENT_COVERAGE')
        for proof in proofs.values():
            fresh(proof['captured_at'], self.now())
            require(proof.get('all_instances_accounted_for') is True and proof.get('external_writers_quiesced') is True
                    and proof.get('activation_operators_excluded') is True and proof.get('authority_reference'), 'QUIESCENCE_UNPROVEN')

    def instances(self, placement, recorded):
        if placement['adapter'] == 'owner-orchestrator-snapshot':
            require(recorded.get('orchestrator_source') and recorded.get('snapshot_sha256'), 'EXTERNAL_ORCHESTRATOR_PROVENANCE')
            require(hashlib.sha256(json.dumps(recorded['instances'],sort_keys=True,separators=(',',':')).encode()).hexdigest() == recorded['snapshot_sha256'], 'SNAPSHOT_DIGEST')
            return recorded['instances']
        require(placement['adapter'] == 'docker-compose', 'PLACEMENT_ADAPTER')
        project = placement['project']
        require(NAME.fullmatch(project), 'PROJECT_ID')
        ids = self.command(['docker', 'ps', '-aq', '--filter', 'label=com.docker.compose.project=' + project]).split()
        require(ids and all(re.fullmatch('[0-9a-f]{12,64}', x) for x in ids), 'ORCHESTRATOR_INVENTORY_MISSING')
        # Restrict inspect to identity/state. Never return environment or secrets.
        fmt = '{{json .Id}} {{json .Image}} {{json .State.Status}} {{json .State.StartedAt}} {{json (index .Config.Labels "com.docker.compose.service")}} {{json .State.Paused}} {{json .State.Restarting}}'
        result = []
        for identity in ids:
            raw = self.command(['docker', 'inspect', '--format', fmt, identity]).strip()
            result.append(docker_instance(raw))
        return result

    def workflow_identity(self, placement, instance, recorded):
        if placement['adapter'] == 'owner-orchestrator-snapshot':
            events = recorded.get('workflow_startup_events', {}).get(instance['id'], [])
        else:
            raw = self.command(['docker', 'logs', '--since', instance['started_at'], instance['id']], include_stderr=True)
            events = []
            for line in raw.splitlines():
                if 'Workflow worker capability identity' not in line:
                    continue
                # Existing tracing plain/JSON format; extract only the UUID.
                matches = re.findall(r'instance_id["\s]*[:=]["\s]*([0-9a-f-]{36})', re.sub(r'\x1b\[[0-9;]*m', '', line))
                events.extend(matches)
        require(len(events) == 1, 'WORKFLOW_PROCESS_CORRELATION_MISSING_OR_AMBIGUOUS')
        return str(uuid.UUID(events[0]))

    def heartbeat(self, gate_id, identity):
        uuid.UUID(identity)
        raw = self.sql(self.by_id[gate_id], "SELECT row_to_json(r) FROM (SELECT instance_id,binary_version,supported_profiles,admits_profiles,heartbeat_ts,extract(epoch FROM clock_timestamp()-heartbeat_ts)::double precision age_seconds FROM workflow_ops.workflow_worker_capability_t " + f"WHERE instance_id='{identity}'::uuid) r;")
        require(raw, 'HEARTBEAT_MISSING')
        return json.loads(raw)

    def write_startup_marker(self, state_dir):
        # Marker names are relative to the mounted operations root in bootstrap.
        content = sha(self.root / 'w7-assets.json') + '  w7-assets.json\n'
        content += sha(self.root / 'bundle/bundle.sha256') + '  bundle/bundle.sha256\n'
        content += sha(Path(state_dir) / 'prepared.json') + '  .runtime/w7/prepared.json\n'
        for relative in ('w7-ownership-v1.json', 'bin/w7_rollout.py', 'bin/w7_ownership.py', 'bin/bundle_contract.py'):
            content += sha(self.root / relative) + '  ' + relative + '\n'
        for path in self.pins['files']:
            if path.startswith('portal/'):
                content += sha(self.root / path) + '  ' + path + '\n'
        temporary = Path(state_dir) / 'startup-ready.tmp'
        with temporary.open('w') as stream:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, Path(state_dir) / 'startup-ready.sha256')


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['prepare', 'activate', 'rollback', 'status', 'catalog', 'recover-off', 'restart-check', 'verify-assets', 'ownership-check', 'ownership-transition'])
    parser.add_argument('--assets', required=True)
    parser.add_argument('--plan')
    parser.add_argument('--evidence')
    parser.add_argument('--state-dir')
    parser.add_argument('--owner-run', action='store_true')
    args = parser.parse_args(argv)
    if args.action == 'verify-assets':
        verify_assets(args.assets)
        print('W7_ASSETS_VERIFIED')
        return 0
    require(args.owner_run and args.plan and args.state_dir, 'OWNER_RUN_INPUTS_REQUIRED')
    require(Path(args.state_dir).resolve() == (Path(args.assets) / '.runtime/w7').resolve(), 'SHARED_STATE_DIRECTORY_REQUIRED')
    plan = load_json(args.plan)
    validate_plan(plan)
    backend = LiveBackend(plan, sha(args.plan), args.assets)
    evidence = load_json(args.evidence) if args.evidence else {}
    coordinator = Coordinator(plan, backend, args.assets, args.state_dir, evidence)
    with serialized(args.state_dir):
        coordinator.event('OPERATION_' + args.action.upper())
        try:
            if args.action == 'catalog':
                print(json.dumps({g['id']: backend.catalog_fingerprint(g, backend.claim_present(g)) for g in plan['gates']}))
            elif args.action != 'status':
                getattr(coordinator, args.action.replace('-', '_'))()
            return 0
        finally:
            states = coordinator.states()
            coordinator.record['states'] = states
            coordinator.record['global_off'] = all(v == 'OFF' for v in states.values())
            atomic_json(Path(args.state_dir) / 'operation.json', coordinator.record)
            print(json.dumps({'states': states, 'global_off': coordinator.record['global_off']}))


def cli(argv=None):
    try:
        return main(argv)
    except (Refusal, OwnershipError) as error:
        print('W7_REFUSED:' + str(error), file=sys.stderr)
        return 2
    except Exception:
        print('W7_REFUSED:UNAVAILABLE_OR_INVALID_INPUT', file=sys.stderr)
        return 2


if __name__ == '__main__':
    def interrupted(_signal, _frame):
        raise Refusal('INTERRUPTED_RECOVER_BY_READBACK')
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    sys.exit(cli())
