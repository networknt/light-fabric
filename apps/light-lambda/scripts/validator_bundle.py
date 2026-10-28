#!/usr/bin/env python3
"""Non-executing qualification bundle packaging, not a runtime trust selector.

The caller must supply an independently reviewed catalogue digest. Never learn
new expected identities from input files. Reuse baseline ZIP rules/serialization;
do not call its bootstrap validation (enabled Validator is deliberately closed).
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import zipfile

import package_baseline

SCHEMA = 'validator-qualification-bundle/v1'
MANIFEST = 'validator-bundle.json'
ROLES = {
    'worker': ('worker/precision-worker', 0o755),
    'source': ('config/openapi.yaml', 0o644),
    'settings': ('config/openapi-validator.yml', 0o644),
}
MAX_MANIFEST = 64 * 1024
MAX_FILE = 32 * 1024 * 1024
MAX_TOTAL = 32 * 1024 * 1024


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def capture(path, limit):
    path = Path(path)
    if not path.is_absolute() or '..' in path.parts:
        raise ValueError('absolute input path required')
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
    with os.fdopen(fd, 'rb') as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or not 0 < info.st_size <= limit:
            raise ValueError('regular bounded input required')
        raw = stream.read(info.st_size + 1)
        if len(raw) != info.st_size:
            raise ValueError('input changed during capture')
    return raw


def catalogue(raw, expected_sha):
    if not re.fullmatch('[0-9a-f]{64}', expected_sha) or digest(raw) != expected_sha:
        raise ValueError('reviewed catalogue digest mismatch')
    # Duplicate JSON keys must not be silently overwritten.
    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError('duplicate catalogue key')
            result[key] = value
        return result
    value = json.loads(raw, object_pairs_hook=unique)
    if set(value) != {'schema', 'profile', 'target', 'sourceFormat', 'files'}:
        raise ValueError('catalogue fields')
    if value['schema'] != SCHEMA or value['target'] != 'linux-x86_64-static-worker' or value['sourceFormat'] != 'yaml':
        raise ValueError('catalogue profile')
    if not isinstance(value['profile'], str) or not re.fullmatch('[a-z0-9][a-z0-9/-]{0,95}', value['profile']):
        raise ValueError('catalogue identity')
    if not isinstance(value['files'], dict) or set(value['files']) != set(ROLES):
        raise ValueError('catalogue roles')
    total = 0
    for role, (name, mode) in ROLES.items():
        row = value['files'][role]
        if not isinstance(row, dict) or set(row) != {'member', 'mode', 'bytes', 'sha256'}:
            raise ValueError('catalogue entry fields')
        if row['member'] != name or type(row['mode']) is not int or row['mode'] != mode:
            raise ValueError('catalogue entry path/mode')
        if type(row['bytes']) is not int or not 0 < row['bytes'] <= MAX_FILE:
            raise ValueError('catalogue entry size')
        if not isinstance(row['sha256'], str) or not re.fullmatch('[0-9a-f]{64}', row['sha256']):
            raise ValueError('catalogue entry digest')
        total += row['bytes']
    if total > MAX_TOTAL:
        raise ValueError('catalogue aggregate limit')
    return value


def check_bytes(raw, row):
    if len(raw) != row['bytes'] or digest(raw) != row['sha256']:
        raise ValueError('pinned content mismatch')


def build(catalogue_path, expected_sha, inputs, output, stamp):
    raw = capture(catalogue_path, MAX_MANIFEST)
    value = catalogue(raw, expected_sha)
    if set(inputs) != set(ROLES):
        raise ValueError('exact input roles required')
    # ZIP timestamp must be an explicitly selected reproducible whole second.
    if type(stamp) is not int or not 315532800 <= stamp <= 4354819198 or stamp % 2:
        raise ValueError('ZIP epoch must fit range and two-second precision')
    files, modes = {MANIFEST: raw}, {MANIFEST: 0o644}
    for role, row in value['files'].items():
        data = capture(inputs[role], row['bytes'])
        check_bytes(data, row)
        files[row['member']], modes[row['member']] = data, row['mode']
    output = Path(output)
    # All inputs admitted before output creation. Never overwrite old evidence.
    with output.open('xb') as stream:
        package_baseline.canonical_zip(stream, files, modes, stamp)
    return verify(output, expected_sha)


def verify(package, expected_sha):
    with zipfile.ZipFile(package) as archive:
        names = package_baseline.safe_entries(archive)
        if names != {MANIFEST, *(name for name, _ in ROLES.values())}:
            raise ValueError('exact bundle members required')
        info = archive.getinfo(MANIFEST)
        if info.file_size > MAX_MANIFEST:
            raise ValueError('catalogue size limit')
        value = catalogue(archive.read(info), expected_sha)
        if stat.S_IMODE(info.external_attr >> 16) != 0o644:
            raise ValueError('catalogue mode')
        for row in value['files'].values():
            info = archive.getinfo(row['member'])
            if info.file_size != row['bytes'] or stat.S_IMODE(info.external_attr >> 16) != row['mode']:
                raise ValueError('bundle size/mode mismatch')
            check_bytes(archive.read(info), row)
    return {'profile': value['profile'], 'catalogueSha256': expected_sha,
            'filesVerified': 3, 'workerExecuted': False,
            'runtimeAdmissionProved': False, 'lambdaPackageQualified': False,
            'productionResourceBoundsProved': False, 'defaultActivation': 'closed'}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['build', 'verify'])
    parser.add_argument('--catalogue', type=Path)
    parser.add_argument('--catalogue-sha256', required=True)
    parser.add_argument('--worker', type=Path)
    parser.add_argument('--source', type=Path)
    parser.add_argument('--settings', type=Path)
    parser.add_argument('--package', type=Path, required=True)
    parser.add_argument('--source-date-epoch', type=int)
    args = parser.parse_args()
    if args.mode == 'build':
        if None in (args.catalogue, args.worker, args.source, args.settings, args.source_date_epoch):
            parser.error('build requires catalogue, three inputs and explicit epoch')
        result = build(args.catalogue, args.catalogue_sha256,
                       {'worker': args.worker, 'source': args.source, 'settings': args.settings},
                       args.package, args.source_date_epoch)
    else:
        result = verify(args.package, args.catalogue_sha256)
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    main()
