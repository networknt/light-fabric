"""E04 ownership v1 companion. No historical migration SQL is modified.

Qualification references are owner-reviewed, content-addressed local artifacts,
not a trust assertion about an arbitrary live catalog. All database access goes
through the existing identity-checked backend. There is no qualification bypass.
"""
import hashlib
import json
from pathlib import Path
import re


class OwnershipError(Exception):
    """Fixed diagnostic only; never embed private connection inputs."""


TABLES = ('wf_definition_t', 'wf_definition_version_t', 'process_info_t')
FUNCTION = 'workflow_claim_host_task_v1(uuid,integer)'
OBJECTS = tuple('workflow_ops.' + x for x in TABLES) + ('workflow_ops.' + FUNCTION,)
CHECKS = ('baseline_preservation', 'ownership_transition', 'ownership_refusals',
          'authority_and_default_acls', 'reserved_key_preflight',
          'forward_and_repeat', 'all_gate_input_refusal', 'ledger_and_state_refusal',
          'interruption_recovery', 'rollback_and_lost_response',
          'receipt_retention', 'independent_session_locks',
          'w2_schema_and_serialization', 'operational_admission_postgres',
          'portal_delivery_replay_postgres', 'w4_w5_w6_postgres',
          'controller_pagination_and_index', 'worker_capability_postgres',
          'agent_runner_cleanup')
DIGEST = re.compile(r'sha256:[0-9a-f]{64}\Z')


def demand(value, code):
    if not value:
        raise OwnershipError(code)


def digest(value):
    return 'sha256:' + hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':')).encode()).hexdigest()


def file_digest(path):
    return 'sha256:' + hashlib.sha256(Path(path).read_bytes()).hexdigest()


def read_json(path):
    try:
        p = Path(path)
        demand(p.is_file() and not p.is_symlink() and p.stat().st_size <= 4 * 1024 * 1024,
               'OWNERSHIP_INPUT_INVALID')
        return json.loads(p.read_text())
    except (OSError, ValueError, TypeError):
        raise OwnershipError('OWNERSHIP_INPUT_INVALID') from None


def verify_companion(root):
    root = Path(root)
    path = root / 'w7-ownership-v1.json'
    data = read_json(path)
    demand(data.get('version') == 1 and data.get('objects') == list(OBJECTS), 'OWNERSHIP_ALLOWLIST_CHANGED')
    demand(set(data.get('files', {})) == {'bin/w7_rollout.py', 'bin/w7_ownership.py'}, 'OWNERSHIP_TOOL_SET')
    for name, expected in data['files'].items():
        demand(file_digest(root / name) == expected, 'OWNERSHIP_TOOL_CHANGED')
    return file_digest(path)


class OwnershipContract:
    def __init__(self, backend):
        self.backend = backend
        self.root = backend.root

    def snapshot_sql(self, gate):
        # Stable named identities, no OIDs or timestamps. Include untouched
        # workflow objects as well, so an ownership transition cannot hide drift.
        s = gate['schema']
        selected = "(c.relname LIKE 'workflow_%' OR c.relname LIKE 'wf_%' OR c.relname IN ('process_info_t','task_info_t'))"
        return f"""SELECT jsonb_build_object(
 'relations',(SELECT coalesce(jsonb_agg(x ORDER BY x.name),'[]') FROM
  (SELECT c.relname name,c.relkind kind,pg_get_userbyid(c.relowner) owner,c.relacl::text acl,
          c.relrowsecurity rls,c.relforcerowsecurity force_rls,c.relpersistence persistence
   FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='{s}' AND {selected}) x),
 'columns',(SELECT coalesce(jsonb_agg(x ORDER BY x.table_name,x.num),'[]') FROM
  (SELECT c.relname table_name,a.attnum num,a.attname name,format_type(a.atttypid,a.atttypmod) type,
          a.attnotnull required,a.attacl::text acl,a.attidentity identity,a.attgenerated generated,
          pg_get_expr(d.adbin,d.adrelid) default_expr
   FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid JOIN pg_namespace n ON n.oid=c.relnamespace
   LEFT JOIN pg_attrdef d ON d.adrelid=c.oid AND d.adnum=a.attnum
   WHERE n.nspname='{s}' AND {selected} AND a.attnum>0 AND NOT a.attisdropped) x),
 'constraints',(SELECT coalesce(jsonb_agg(x ORDER BY x.table_name,x.name),'[]') FROM
  (SELECT c.relname table_name,k.conname name,pg_get_constraintdef(k.oid) definition,k.convalidated valid
   FROM pg_constraint k JOIN pg_class c ON c.oid=k.conrelid JOIN pg_namespace n ON n.oid=c.relnamespace
   WHERE n.nspname='{s}' AND {selected}) x),
 'indexes',(SELECT coalesce(jsonb_agg(x ORDER BY x.name),'[]') FROM
  (SELECT i.relname name,pg_get_indexdef(i.oid) definition,pg_get_userbyid(i.relowner) owner,
          k.indisvalid valid,k.indisready ready FROM pg_index k JOIN pg_class c ON c.oid=k.indrelid
   JOIN pg_class i ON i.oid=k.indexrelid JOIN pg_namespace n ON n.oid=c.relnamespace
   WHERE n.nspname='{s}' AND {selected}) x),
 'triggers',(SELECT coalesce(jsonb_agg(x ORDER BY x.table_name,x.name),'[]') FROM
  (SELECT c.relname table_name,t.tgname name,pg_get_triggerdef(t.oid) definition,t.tgenabled enabled
   FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid JOIN pg_namespace n ON n.oid=c.relnamespace
   WHERE n.nspname='{s}' AND {selected} AND NOT t.tgisinternal) x),
 'functions',(SELECT coalesce(jsonb_agg(x ORDER BY x.name,x.arguments),'[]') FROM
  (SELECT p.proname name,pg_get_function_identity_arguments(p.oid) arguments,pg_get_functiondef(p.oid) definition,
          pg_get_userbyid(p.proowner) owner,p.proacl::text acl,p.prosecdef security_definer,p.proconfig config
   FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace
   WHERE n.nspname='{s}' AND p.proname LIKE 'workflow_%' AND p.prokind='f') x),
 'defaults',(SELECT coalesce(jsonb_agg(x ORDER BY x.owner,x.kind),'[]') FROM
  (SELECT pg_get_userbyid(d.defaclrole) owner,d.defaclobjtype kind,d.defaclacl::text acl
   FROM pg_default_acl d JOIN pg_namespace n ON n.oid=d.defaclnamespace WHERE n.nspname='{s}') x),
 'attached',(SELECT coalesce(jsonb_agg(x ORDER BY x.table_name),'[]') FROM
  (SELECT c.relname table_name,pg_get_userbyid(t.typowner) row_type_owner,
   CASE WHEN c.reltoastrelid<>0 THEN pg_get_userbyid(z.relowner) END toast_owner,
   (SELECT array_agg(pg_get_userbyid(i.relowner) ORDER BY i.relname) FROM pg_index k
    JOIN pg_class i ON i.oid=k.indexrelid WHERE k.indrelid=c.reltoastrelid) toast_index_owners
   FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
   JOIN pg_type t ON t.oid=c.reltype LEFT JOIN pg_class z ON z.oid=c.reltoastrelid
   WHERE n.nspname='{s}' AND c.relkind='r' AND {selected}) x),
 'schema_owner',(SELECT pg_get_userbyid(nspowner) FROM pg_namespace WHERE nspname='{s}'))::text;"""

    def snapshot(self, gate):
        self.backend.identity(gate)
        return json.loads(self.backend.sql(gate, self.snapshot_sql(gate)))

    def stage(self, gate):
        rows = self.backend.migrations(gate)
        if gate['kind'] == 'operational':
            recorded_rows = json.loads(self.backend.sql(gate, "SELECT coalesce(json_agg(x),'[]') FROM (SELECT migration_owner, schema_name, migration_id, migration_digest FROM operational_meta.operational_schema_migration_t ORDER BY migration_owner,schema_name,migration_id) x;"))
            recorded_map = {(r['migration_owner'], r['schema_name'], r['migration_id']): r['migration_digest'].removeprefix('sha256:') for r in recorded_rows}
            demand(set(recorded_map) <= {(r[3],r[4],r[0]) for r in rows}, 'OWNERSHIP_UNKNOWN_LEDGER_ENTRY')
        else:
            recorded_rows = json.loads(self.backend.sql(gate, "SELECT coalesce(json_agg(x),'[]') FROM (SELECT patch_id,checksum FROM public.portal_schema_patch_t ORDER BY patch_id) x;"))
            recorded_map = {('portal','public',r['patch_id']):r['checksum'] for r in recorded_rows}
        applied = []
        missing_seen = False
        for mid, _path, expected, owner, schema in rows:
            recorded = recorded_map.get((owner,schema,mid), '')
            is_e04 = gate['kind'] == 'portal' or mid in ('0024_workflow_expression_profile', '0025_workflow_operation_receipts')
            if not is_e04:
                demand(recorded == expected, 'OWNERSHIP_BASELINE_LEDGER')
                continue
            if recorded:
                demand(not missing_seen and recorded == expected, 'OWNERSHIP_E04_LEDGER')
                applied.append(mid)
            else:
                missing_seen = True
        return len(applied)

    def reference(self, gate):
        companion = verify_companion(self.root)
        ref = gate.get('qualification_reference')
        demand(isinstance(ref, dict) and set(ref) == {'path', 'sha256'}, 'QUALIFICATION_REFERENCE_REQUIRED')
        demand(DIGEST.fullmatch(ref['sha256']) is not None, 'QUALIFICATION_REFERENCE_DIGEST')
        try:
            demand(file_digest(ref['path']) == ref['sha256'], 'QUALIFICATION_REFERENCE_CHANGED')
        except OSError:
            raise OwnershipError('QUALIFICATION_REFERENCE_UNAVAILABLE') from None
        q = read_json(ref['path'])
        demand(q.get('version') == 1 and q.get('status') == 'qualified', 'QUALIFICATION_NOT_COMPLETE')
        demand(q.get('companion_sha256') == companion and
               q.get('sql_assets_sha256') == file_digest(self.root / 'w7-assets.json'), 'QUALIFICATION_TOOL_ASSET_MISMATCH')
        demand(q.get('database') == gate['database'] and q.get('schema') == gate['schema'] and
               q.get('scope_root_id') == gate.get('scope_root_id'), 'QUALIFICATION_CONFIGURATION_MISMATCH')
        checks = q.get('checks', {})
        demand(set(checks) == set(CHECKS) and all(v == 'PASS' for v in checks.values()), 'QUALIFICATION_LANES_INCOMPLETE')
        evidence = q.get('evidence', {})
        demand(isinstance(evidence, dict) and evidence, 'QUALIFICATION_EVIDENCE_MISSING')
        base = Path(ref['path']).resolve().parent
        for name, expected in evidence.items():
            p = (base / name).resolve()
            demand(p.is_relative_to(base) and p.is_file() and file_digest(p) == expected, 'QUALIFICATION_EVIDENCE_CHANGED')
        for field in ('catalog_fingerprint', 'rollback_catalog_fingerprint'):
            demand(DIGEST.fullmatch(q.get(field, '')) is not None and gate.get(field) == q[field], 'QUALIFICATION_FINGERPRINT_INPUT')
        owner = 'portal' if gate['kind'] == 'portal' else 'migrator'
        prefixes = q.get('preinstall', {})
        demand(all(isinstance(prefixes.get(f'{i}:{owner}'), str) and
                   DIGEST.fullmatch(prefixes[f'{i}:{owner}']) for i in range(3)),
               'QUALIFICATION_PREFIXES_INCOMPLETE')
        if '0:historical' in prefixes:
            demand(DIGEST.fullmatch(prefixes['0:historical']) and
                   digest(q.get('transition_post_snapshot')) == prefixes['0:migrator'],
                   'OWNERSHIP_POSTCONDITION_REQUIRED')
        return q

    def authority(self, gate, snapshot, transition=False):
        b = self.backend
        if gate['kind'] != 'operational':
            demand(b.sql(gate, "SELECT has_schema_privilege(current_user,'public','CREATE') AND has_table_privilege(current_user,'public.portal_schema_patch_t','SELECT,INSERT');") == 't', 'PORTAL_MIGRATION_AUTHORITY')
            demand(b.sql(gate, "SELECT bool_and(pg_has_role(current_user,c.relowner,'USAGE')) FROM pg_class c WHERE c.oid IN ('public.wf_definition_t'::regclass,'public.wf_definition_version_t'::regclass,'public.process_info_t'::regclass);") == 't', 'PORTAL_OBJECT_AUTHORITY')
            return 'portal'
        db = gate['database']; migrator = db + '_workflow_migrator'; runtime = db + '_workflow_runtime'
        roles = json.loads(b.sql(gate, f"""SELECT json_agg(x) FROM (SELECT rolname,rolsuper,rolcreatedb,rolcreaterole,rolreplication,rolbypassrls FROM pg_roles WHERE rolname IN ('{migrator}','{runtime}')) x;"""))
        demand(roles and len(roles) == 2 and all(not any(v for k,v in r.items() if k != 'rolname') for r in roles), 'WORKFLOW_ROLE_UNSAFE')
        demand(snapshot['schema_owner'] == migrator, 'WORKFLOW_SCHEMA_OWNER')
        demand(b.sql(gate, f"SELECT NOT pg_has_role('{runtime}','{migrator}','MEMBER') AND NOT has_schema_privilege('{runtime}','workflow_ops','CREATE') AND NOT has_database_privilege('{runtime}',current_database(),'CREATE');") == 't', 'RUNTIME_DDL_AUTHORITY')
        demand(b.sql(gate, f"SELECT NOT EXISTS(SELECT 1 FROM pg_roles WHERE rolname<>'{runtime}' AND pg_has_role('{runtime}',oid,'MEMBER'));") == 't', 'RUNTIME_ROLE_MEMBERSHIP')
        demand(b.sql(gate, f"SELECT NOT EXISTS(SELECT 1 FROM pg_roles WHERE rolname<>'{migrator}' AND pg_has_role('{migrator}',oid,'MEMBER'));") == 't', 'MIGRATOR_ROLE_MEMBERSHIP')
        demand(b.sql(gate, f"SELECT pg_has_role(session_user,'{migrator}','SET') AND has_table_privilege(current_user,'operational_meta.operational_schema_migration_t','SELECT,INSERT');") == 't', 'MIGRATION_OPERATOR_AUTHORITY')
        # The SET operation is read-only and proves the actual connection can use it.
        demand(b.sql(gate, f"BEGIN READ ONLY; SET LOCAL ROLE {migrator}; SELECT current_user; ROLLBACK;") == migrator, 'MIGRATION_SET_ROLE')
        rels = {r['name']:r for r in snapshot['relations']}
        funcs = [f for f in snapshot['functions'] if f['name'] == 'workflow_claim_host_task_v1']
        demand(all(t in rels and rels[t]['kind'] == 'r' and not rels[t]['rls'] and not rels[t]['force_rls'] for t in TABLES)
               and len(funcs) == 1 and not funcs[0]['security_definer'], 'OWNERSHIP_OBJECT_KIND_OR_SECURITY')
        owners = {rels[t]['owner'] for t in TABLES} | {funcs[0]['owner']}
        demand(owners in ({'postgres'}, {migrator}), 'OWNERSHIP_MIXED_OR_UNEXPECTED')
        demand(transition or owners == {migrator}, 'OWNERSHIP_TRANSITION_REQUIRED')
        if transition and owners == {'postgres'}:
            demand(b.sql(gate, "SELECT rolsuper FROM pg_roles WHERE rolname=current_user;") == 't', 'OWNERSHIP_ADMIN_REQUIRED')
        # No free-standing owned sequence or partition/inheritance graph can be
        # silently swept into this small ownership operation.
        demand(b.sql(gate, """SELECT NOT EXISTS(SELECT 1 FROM pg_depend d JOIN pg_class c ON c.oid=d.objid AND d.classid='pg_class'::regclass WHERE c.relkind='S' AND d.refobjid IN ('workflow_ops.wf_definition_t'::regclass,'workflow_ops.wf_definition_version_t'::regclass,'workflow_ops.process_info_t'::regclass)) AND NOT EXISTS(SELECT 1 FROM pg_inherits WHERE inhrelid IN ('workflow_ops.wf_definition_t'::regclass,'workflow_ops.wf_definition_version_t'::regclass,'workflow_ops.process_info_t'::regclass) OR inhparent IN ('workflow_ops.wf_definition_t'::regclass,'workflow_ops.wf_definition_version_t'::regclass,'workflow_ops.process_info_t'::regclass));""") == 't', 'OWNERSHIP_DEPENDENCY_UNEXPECTED')
        return 'historical' if owners == {'postgres'} else 'migrator'

    def validate(self, gate, transition=False):
        q = self.reference(gate)
        stage = self.stage(gate)
        state = self.backend.state(gate)
        demand(state == ('ABSENT' if stage == 0 else 'OFF'), 'PREINSTALL_ADMISSION_STATE')
        snapshot = self.snapshot(gate)
        owner_class = self.authority(gate, snapshot, transition)
        key = str(stage) + ':' + owner_class
        expected = q.get('preinstall', {}).get(key)
        demand(expected and digest(snapshot) == expected, 'PREINSTALL_CATALOG_MISMATCH')
        if owner_class == 'historical':
            demand(stage == 0, 'OWNERSHIP_TRANSITION_PARTIAL_E04')
        return {'stage': stage, 'owner_class': owner_class, 'snapshot': snapshot,
                'snapshot_sha256': digest(snapshot), 'qualification': q}

    def transition_sql(self, gate, validated):
        # Literal SQL targets: never expand this list from a plan or catalog.
        db = gate['database']; target = db + '_workflow_migrator'
        demand(validated['stage'] == 0 and validated['owner_class'] == 'historical', 'OWNERSHIP_TRANSITION_STATE')
        before = json.dumps(validated['snapshot'], sort_keys=True, separators=(',', ':'))
        demand('$e04_snapshot$' not in before, 'OWNERSHIP_SNAPSHOT_QUOTING')
        query = self.snapshot_sql(gate).strip().removesuffix(';')
        post = validated['qualification']['preinstall'].get('0:migrator')
        demand(post and DIGEST.fullmatch(post), 'OWNERSHIP_POSTCONDITION_REQUIRED')
        post_snapshot = validated['qualification'].get('transition_post_snapshot')
        demand(post_snapshot and digest(post_snapshot) == post, 'OWNERSHIP_POSTCONDITION_REQUIRED')
        after = json.dumps(post_snapshot, sort_keys=True, separators=(',', ':'))
        demand('$e04_snapshot$' not in after, 'OWNERSHIP_SNAPSHOT_QUOTING')
        # Compare JSONB in the same transaction after locks, before ALTER OWNER.
        return f"""BEGIN;
LOCK TABLE workflow_ops.wf_definition_t,workflow_ops.wf_definition_version_t,workflow_ops.process_info_t IN ACCESS EXCLUSIVE MODE;
DO $e04_owner$ DECLARE actual jsonb; BEGIN
 SELECT q.value::jsonb INTO actual FROM ({query}) AS q(value);
 IF actual IS DISTINCT FROM $e04_snapshot${before}$e04_snapshot$::jsonb THEN RAISE EXCEPTION 'OWNERSHIP_CHANGED_UNDER_LOCK'; END IF;
END $e04_owner$;
ALTER TABLE workflow_ops.wf_definition_t OWNER TO {target};
ALTER TABLE workflow_ops.wf_definition_version_t OWNER TO {target};
ALTER TABLE workflow_ops.process_info_t OWNER TO {target};
ALTER FUNCTION workflow_ops.workflow_claim_host_task_v1(uuid,integer) OWNER TO {target};
DO $e04_owner$ DECLARE actual jsonb; BEGIN
 SELECT q.value::jsonb INTO actual FROM ({query}) AS q(value);
 IF actual IS DISTINCT FROM $e04_snapshot${after}$e04_snapshot$::jsonb THEN RAISE EXCEPTION 'OWNERSHIP_POSTCONDITION_MISMATCH'; END IF;
END $e04_owner$;
COMMIT;"""
