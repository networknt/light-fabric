"""Exact reviewed operational bundle identities; never accepts arbitrary versions."""
import hashlib
import json
from pathlib import Path

# Maintained literals are populated from the reviewed 2.6.0 and 2.7.0 artifacts.
CONTRACTS = {'2.6.0': {'count': 44,
           'identity': {'bundle.sha256': 'a114c5cab41eced34e05c137031f922e2b392c7ac750fcd2f914817410cb589a',
                        'manifest.json': 'c632fcfebd57451aac2b485b4423240fe13dec74766993c8b94b120a6215e1d2',
                        'migration-order.tsv': '2eb52eebd7a91dae2b36c48cb99807853773c567f7f69fa400eeb2215c7085a6'}},
 '2.7.0': {'count': 45,
           'identity': {'bundle.sha256': '461c90e5c46acb31550c6ecc16786a397625655ef3ad62d45c7e0be6844ac2af',
                        'manifest.json': 'ac8bc08e2d6f1d75f4f27f07f8e7ed8c7766854d1be833c04370bcae0b1cb295',
                        'migration-order.tsv': '97194db5d36ad8bbfbfa8c24eab43cf465d6906daaa9f671b6031282c69f18a1'}}}
CURRENT_VERSION = '2.7.0'

def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()

def verify_bundle(root):
    root = Path(root)
    manifest = json.loads((root / 'manifest.json').read_text())
    version = manifest['bundleVersion']
    if version not in CONTRACTS:
        raise ValueError('UNREVIEWED_BUNDLE_VERSION')
    contract = CONTRACTS[version]
    for name, expected in contract['identity'].items():
        if digest(root / name) != expected:
            raise ValueError('BUNDLE_IDENTITY_MISMATCH')
    listed = {}
    for line in (root / 'bundle.sha256').read_text().splitlines():
        expected, relative = line.split('  ', 1)
        path = root / relative
        if relative in listed or not path.resolve().is_relative_to(root.resolve()) or path.is_symlink():
            raise ValueError('BUNDLE_FILE_IDENTITY')
        if digest(path) != expected:
            raise ValueError('BUNDLE_ASSET_MISMATCH')
        listed[relative] = expected
    actual = {str(p.relative_to(root)) for p in root.rglob('*') if p.is_file() or p.is_symlink()}
    if actual != set(listed) | {'bundle.sha256'}:
        raise ValueError('BUNDLE_FILE_SET')
    rows = []
    for line in (root / 'migration-order.tsv').read_text().splitlines():
        if not line or line.startswith('#'):
            continue
        order, owner, schema, migration, relative, expected = line.split('\t')
        rows.append((int(order), owner, schema, migration, relative, expected))
    if len(rows) != contract['count'] or [r[0] for r in rows] != list(range(1, contract['count'] + 1)):
        raise ValueError('BUNDLE_MIGRATION_ORDER')
    if len(manifest['orderedMigrations']) != len(rows):
        raise ValueError('BUNDLE_MANIFEST_ORDER')
    for row, entry in zip(rows, manifest['orderedMigrations']):
        if row != tuple(entry[k] for k in ('order', 'owner', 'schema', 'migrationId', 'path', 'sha256')) or listed.get(row[4]) != row[5]:
            raise ValueError('BUNDLE_MANIFEST_ORDER')
    return version, rows

if __name__ == '__main__':
    import sys
    try:
        version, rows = verify_bundle(sys.argv[1])
        print('REVIEWED_BUNDLE_VERIFIED:' + version + ':' + str(len(rows)))
    except (OSError, ValueError, KeyError):
        raise SystemExit('REVIEWED_BUNDLE_REFUSED') from None
