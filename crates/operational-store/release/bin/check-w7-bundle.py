#!/usr/bin/env python3
"""Static current-bundle gate; historical W2 checker remains unchanged."""
import argparse
import json
from pathlib import Path
from w7_rollout import require, sha, verify_assets, Refusal
from w7_ownership import verify_companion, OwnershipError


def check(root, fabric=None, portal=None, copies=()):
    root = Path(root)
    pins, rows = verify_assets(root)
    for copy in copies:
        other, _ = verify_assets(copy)
        require(pins == other, 'DEPLOYMENT_COPY_IDENTITY')
        require(verify_companion(root) == verify_companion(copy), 'OWNERSHIP_COMPANION_COPY_IDENTITY')
    if fabric:
        for _, _, _, _, relative, digest in rows:
            require(sha(Path(fabric) / relative) == digest, 'CANONICAL_SOURCE_MISMATCH')
    if portal:
        portal = Path(portal) / 'postgres'
        inventory = json.loads((portal / 'migrations/inventory.json').read_text())
        for relative, digest in pins['files'].items():
            if relative.startswith('portal/'):
                require(sha(portal / relative.removeprefix('portal/')) == digest, 'PORTAL_COPY_MISMATCH')
                if relative.startswith('portal/patch_'):
                    require(inventory[relative.removeprefix('portal/')]['sha256'] == digest, 'PORTAL_REVIEWED_INVENTORY')
    # Independently reviewed rollback files must agree with their checksum files,
    # whose accepted path formats differ between W2 and W3b.
    for folder in [root / 'bundle/rollback', root / 'portal/rollback']:
        for checksum in folder.glob('*.sha256'):
            digest, relative = checksum.read_text().strip().split(None, 1)
            source = root / 'bundle' / relative if folder.parent.name == 'bundle' else root / 'portal' / relative
            if not source.is_file():
                source = folder / relative
            require(source.is_file() and sha(source) == digest, 'ROLLBACK_CHECKSUM')
    print('W7 current bundle: 44 forward migrations, 58 accepted assets; copies and independent rollbacks verified')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--assets', required=True)
    parser.add_argument('--fabric')
    parser.add_argument('--portal')
    parser.add_argument('--copy', action='append', default=[])
    args = parser.parse_args()
    try:
        check(args.assets, args.fabric, args.portal, args.copy)
    except (Refusal, OwnershipError, OSError, ValueError, KeyError):
        raise SystemExit('W7_STATIC_REFUSED') from None
