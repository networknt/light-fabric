#!/usr/bin/env python3
"""Synchronize only the reviewed operational release; never writes readiness."""
import argparse
import hashlib
from pathlib import Path
import shutil
import sys
import uuid
from w7_rollout import verify_assets
from w7_ownership import verify_companion

def sync(source, destination, check=False):
    if Path(destination).is_symlink():
        raise ValueError('SYNC_SYMLINK_TARGET')
    source, destination = Path(source).resolve(), Path(destination).resolve()
    verify_assets(source)
    verify_companion(source)
    if source == destination or destination.is_relative_to(source):
        raise ValueError('SYNC_TARGET_IDENTITY')
    required = [p.relative_to(source) for folder in ['bundle', 'portal', 'bin', 'contracts'] for p in (source / folder).rglob('*') if p.is_file() and '__pycache__' not in p.parts]
    required += [Path('w7-assets.json'), Path('w7-ownership-v1.json')]
    if check:
        for name in required:
            if not (destination / name).is_file() or (source / name).read_bytes() != (destination / name).read_bytes():
                raise ValueError('DEPLOYMENT_COPY_IDENTITY')
        verify_assets(destination)
        verify_companion(destination)
        return
    destination.mkdir(parents=True, exist_ok=True)
    for name in required:
        target = destination / name
        if target.is_symlink() or any(parent.is_symlink() for parent in target.parents if parent != destination):
            raise ValueError('SYNC_SYMLINK_TARGET')
        target.parent.mkdir(parents=True, exist_ok=True)
        temporary = target.with_name(target.name + '.stage-' + uuid.uuid4().hex)
        shutil.copy2(source / name, temporary)
        temporary.replace(target)
    sync(source, destination, True)

if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', required=True)
    parser.add_argument('--destination', required=True)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    sync(args.source, args.destination, args.check)
    print('OPERATIONAL_RELEASE_COPY_VERIFIED')
