#!/usr/bin/env python3
"""Static W2 packaging and exact legacy-claim preservation checks; no database."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--portal-worktree', type=Path, required=True)
    args = parser.parse_args()
    fabric = Path(__file__).resolve().parents[3]
    portal = args.portal_worktree.resolve()
    bundle = fabric / 'crates/operational-store/release/bundle'

    def original(root, name):
        return subprocess.check_output(['rtk', 'proxy', 'git', '-C', str(root), 'show', 'HEAD:' + name], text=True)

    def digest(path):
        return hashlib.sha256(path.read_bytes()).hexdigest()

    previous = json.loads(original(fabric, 'crates/operational-store/release/bundle/manifest.json'))
    current = json.loads((bundle / 'manifest.json').read_text())
    assert previous['bundleVersion'] == '2.4.0' and current['bundleVersion'] == '2.5.0'
    assert current['orderedMigrations'][:-1] == previous['orderedMigrations'], 'historical entries changed'
    assert {k: v for k, v in previous.items() if k not in ('bundleVersion', 'orderedMigrations')} == {k: v for k, v in current.items() if k not in ('bundleVersion', 'orderedMigrations')}
    rows = (bundle / 'migration-order.tsv').read_text()
    assert rows.startswith(original(fabric, 'crates/operational-store/release/bundle/migration-order.tsv'))
    assert len(rows.splitlines()) == len(current['orderedMigrations']) + 1
    for migration in current['orderedMigrations']:
        name = migration['path']
        assert digest(fabric / name) == digest(bundle / name) == migration['sha256']
    for line in original(fabric, 'crates/operational-store/release/bundle/bundle.sha256').splitlines()[2:]:
        assert line in (bundle / 'bundle.sha256').read_text().splitlines(), 'historical digest line changed'
    print('PASS 43 canonical/package migration identities; 42 historical entries preserved')

    # Model the specific additive table/column ACL statements over 0001's
    # actual default grant. This is static regression evidence, not a DB gate.
    baseline = original(fabric, 'crates/workflow-store/migrations/workflow-postgres/0001_workflow_runtime.sql')
    assert re.search(r'ALTER DEFAULT PRIVILEGES FOR ROLE operations_workflow_migrator IN SCHEMA workflow_ops\s+GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO operations_workflow_runtime;', baseline)
    forward = (fabric / 'crates/workflow-store/migrations/workflow-postgres/0024_workflow_expression_profile.sql').read_text()
    policy = 'workflow_expression_profile_policy_t'
    capability = 'workflow_worker_capability_t'

    def effective_acl(source):
        tables = {name: {'SELECT', 'INSERT', 'UPDATE', 'DELETE'} for name in [policy, capability]}
        columns = {name: set() for name in tables}
        for action, privileges, name in re.findall(
            r'^(GRANT|REVOKE) (.+) ON (?:TABLE )?workflow_ops\.(\w+) (?:TO|FROM) operations_workflow_runtime;', source, re.M
        ):
            if name not in tables:
                continue
            if action == 'REVOKE':
                assert privileges == 'ALL', 'unexpected revoke requires ACL model review'
                tables[name].clear()
                columns[name].clear()
            else:
                for privilege in privileges.split(', '):
                    if '(' in privilege:
                        columns[name].add(privilege)
                    else:
                        tables[name].add(privilege)
        return tables, columns

    expected = ({policy: {'SELECT'}, capability: {'SELECT', 'INSERT', 'UPDATE'}},
                {policy: {'UPDATE(profile_id)'}, capability: set()})
    assert effective_acl(forward) == expected, 'baseline defaults defeat intended runtime ACLs'
    for name in [policy, capability]:
        revoke = 'REVOKE ALL ON workflow_ops.' + name + ' FROM operations_workflow_runtime;'
        assert forward.count(revoke) == 1
        assert effective_acl(forward.replace(revoke, '')) != expected, 'ACL regression is insensitive to missing revoke'
    print('PASS additive-default ACL regression; omitting either runtime revoke is detected (static only)')

    def function(source, schema, version):
        return re.search(r'CREATE (?:OR REPLACE )?FUNCTION ' + schema + r'\.workflow_claim_host_task_v' + str(version) + r'\(.*?\n\$\$;', source, re.S).group()

    def undo_approved_filter(source, schema, version):
        source = source.replace('CREATE OR REPLACE FUNCTION', 'CREATE FUNCTION')
        source = source.replace('workflow_claim_host_task_v2', 'workflow_claim_host_task_v1')
        source = source.replace(', p_supported_profiles text[]', '')
        source = source.replace('FROM ' + schema + '.task_info_t t JOIN ' + schema + '.process_info_t p ON p.host_id=t.host_id AND p.process_id=t.process_id', 'FROM task_info_t t')
        predicate = "p.expression_profile='cel-workflow-v1'" if version == 1 else 'p.expression_profile=ANY(p_supported_profiles)'
        source = source.replace('WHERE ' + predicate + ' AND ', 'WHERE ')
        source = source.replace('FOR UPDATE OF t SKIP LOCKED', 'FOR UPDATE SKIP LOCKED')
        for prefix in ['LEFT JOIN ', 'INSERT INTO ', 'UPDATE ']:
            source = source.replace(prefix + schema + '.', prefix)
        return source

    for root, schema, historical, forward in [
        (fabric, 'workflow_ops', 'crates/workflow-store/migrations/workflow-postgres/0021_workflow_durable_timer.sql', 'crates/workflow-store/migrations/workflow-postgres/0024_workflow_expression_profile.sql'),
        (portal, 'public', 'postgres/ddl.sql', 'postgres/patch_20261002_01_workflow_expression_profile.sql'),
    ]:
        old = function(original(root, historical), schema, 1).replace('CREATE OR REPLACE FUNCTION', 'CREATE FUNCTION')
        source = (root / forward).read_text()
        for version in [1, 2]:
            assert undo_approved_filter(function(source, schema, version), schema, version) == old, 'unapproved claim behavior change'
        if schema == 'public':
            ddl = (root / 'postgres/ddl.sql').read_text()
            for version in [1, 2]:
                assert function(ddl, schema, version) == function(source, schema, version), 'fresh/upgrade claim drift'
        print('PASS ' + schema + ' claim v1/v2 exact legacy eligibility/locking/lease/order preservation')

    inventory = json.loads((portal / 'postgres/migrations/inventory.json').read_text())
    prior_inventory = json.loads(original(portal, 'postgres/migrations/inventory.json'))
    assert {k: inventory[k] for k in prior_inventory} == prior_inventory
    patch = 'patch_20261002_01_workflow_expression_profile.sql'
    assert inventory[patch]['sha256'] == digest(portal / 'postgres' / patch)
    for base, rollback in [(bundle, 'rollback/0024_workflow_expression_profile.sql'), (portal / 'postgres', 'rollback/rollback_20261002_01_workflow_expression_profile.sql')]:
        assert (base / (rollback + '.sha256')).read_text() == digest(base / rollback) + '  ' + rollback + '\n'
        assert rollback not in rows and rollback not in json.dumps(current['orderedMigrations'])
    print('PASS Portal historical inventory, new digest and independent out-of-order rollback identities')


if __name__ == '__main__':
    main()
