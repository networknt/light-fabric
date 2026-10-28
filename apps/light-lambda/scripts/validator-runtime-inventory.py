#!/usr/bin/env python3
"""Non-executing ELF dependency inventory; never proof of immutable loading.

Capture requires an independently supplied worker digest and explicit library
paths. Verification never rebaselines hashes. No ldd, loader or worker invocation.
Limits below bound this local inventory only, not production memory usage.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import tempfile

MAX_FILES = 16
MAX_FILE = 32 * 1024 * 1024
MAX_TOTAL = 64 * 1024 * 1024
SCHEMA = 'validator-runtime-inventory/v1'


def elf_metadata(raw):
    if len(raw) < 64 or raw[:7] != b'\x7fELF\x02\x01\x01' or raw[18:20] != b'\x3e\x00':
        raise ValueError('not ELF64 little-endian x86_64')
    # readelf sees exactly these captured bytes, never a mutable source pathname.
    with tempfile.TemporaryDirectory(prefix='validator-elf-data-') as directory:
        data = Path(directory) / 'non-executable-elf'
        data.write_bytes(raw)
        data.chmod(0o600)
        result = subprocess.run(['readelf', '-W', '-l', '-d', '--version-info', str(data)],
                                env={'PATH': os.environ['PATH'], 'LC_ALL': 'C'},
                                capture_output=True, check=True, timeout=20)
    if len(result.stdout) > 1024 * 1024:
        raise ValueError('ELF metadata output limit')
    text = result.stdout.decode('utf-8', errors='strict')
    interpreter = re.findall(r'Requesting program interpreter: ([^\]]+)\]', text)
    if len(interpreter) > 1:
        raise ValueError('multiple interpreters')
    needed = re.findall(r'\(NEEDED\).*Shared library: \[([^\]]+)\]', text)
    if len(needed) != len(set(needed)) or any('/' in n for n in needed):
        raise ValueError('unsupported dependency name')
    soname = re.findall(r'\(SONAME\).*Library soname: \[([^\]]+)\]', text)
    if len(soname) > 1:
        raise ValueError('multiple sonames')
    required, defined = {}, set()
    mode, current = None, None
    for line in text.splitlines():
        if line.startswith('Version needs section'):
            mode = 'need'
        elif line.startswith('Version definition section'):
            mode = 'define'
        elif line.startswith('Version symbols section'):
            mode = None
        if mode == 'need':
            match = re.search(r'File: (\S+)', line)
            if match:
                current = match[1]
                required.setdefault(current, [])
            match = re.search(r'Name: (\S+)', line)
            if match:
                if current is None:
                    raise ValueError('version without dependency')
                required[current].append(match[1])
        elif mode == 'define':
            match = re.search(r'Name: (\S+)', line)
            if match:
                defined.add(match[1])
    return dict(interpreter=interpreter[0] if interpreter else None,
                needed=needed, soname=soname[0] if soname else None,
                requiredVersions={k: sorted(v) for k, v in sorted(required.items())},
                definedVersions=sorted(defined),
                searchPaths=re.findall(r'\((?:RPATH|RUNPATH)\).*\[([^\]]*)\]', text))


def capture_file(path):
    path = Path(path)
    if not path.is_absolute() or '..' in path.parts or path.resolve(strict=True) != path:
        raise ValueError('explicit canonical absolute path required')
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, 'rb') as source:
        info = os.fstat(source.fileno())
        if not stat.S_ISREG(info.st_mode) or not 64 <= info.st_size <= MAX_FILE:
            raise ValueError('file type/size limit')
        raw = source.read(info.st_size + 1)
        if len(raw) != info.st_size:
            raise ValueError('file changed during capture')
    return dict(path=str(path), bytes=len(raw), sha256=hashlib.sha256(raw).hexdigest(),
                elf=elf_metadata(raw))


def check_closure(manifest):
    if manifest.get('schema') != SCHEMA:
        raise ValueError('inventory schema')
    worker, libraries = manifest['worker'], manifest['libraries']
    rows = [worker, *libraries.values()]
    if len(rows) > MAX_FILES or sum(r['bytes'] for r in rows) > MAX_TOTAL:
        raise ValueError('inventory aggregate limit')
    paths = [r['path'] for r in rows]
    if len(paths) != len(set(paths)):
        raise ValueError('duplicate file path')
    interpreter = worker['elf']['interpreter']
    if interpreter is None:
        raise ValueError('this profile requires the captured dynamic interpreter')
    loader = Path(interpreter).name
    if loader not in libraries:
        raise ValueError('missing interpreter pin')
    reached, pending = set(), [worker['elf'], libraries[loader]['elf']]
    reached.add(loader)
    while pending:
        elf = pending.pop()
        if elf['searchPaths']:
            raise ValueError('custom library search path needs separate disposition')
        for name in elf['needed']:
            if name not in libraries:
                raise ValueError('missing dependency pin: ' + name)
            if libraries[name]['elf']['soname'] != name:
                raise ValueError('dependency soname mismatch: ' + name)
            if name not in reached:
                reached.add(name)
                pending.append(libraries[name]['elf'])
        for name, versions in elf['requiredVersions'].items():
            if name not in libraries or not set(versions) <= set(libraries[name]['elf']['definedVersions']):
                raise ValueError('unsatisfied symbol versions: ' + name)
    if reached != set(libraries):
        raise ValueError('unused library pin')
    return sorted(reached)


def capture(worker_path, expected_sha, library_paths):
    if not re.fullmatch('[0-9a-f]{64}', expected_sha):
        raise ValueError('worker digest format')
    if len(library_paths) + 1 > MAX_FILES:
        raise ValueError('inventory file limit')
    worker = capture_file(worker_path)
    if worker['sha256'] != expected_sha:
        raise ValueError('worker digest mismatch')
    result = dict(schema=SCHEMA, worker=worker,
                  libraries={k: capture_file(p) for k, p in sorted(library_paths.items())})
    check_closure(result)
    return result


def verify(manifest):
    check_closure(manifest)
    for row in [manifest['worker'], *manifest['libraries'].values()]:
        if capture_file(row['path']) != row:
            raise ValueError('pinned bytes or ELF metadata changed: ' + row['path'])
    return dict(schema=SCHEMA, filesVerified=1 + len(manifest['libraries']),
                workerSha256=manifest['worker']['sha256'], declaredElfClosureComplete=True,
                workerExecuted=False, immutableRuntimeLoadingProved=False,
                dynamicallyLoadedOrPlatformInjectedObjectsMeasured=False,
                productionActivationApproved=False)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='mode', required=True)
    c = commands.add_parser('capture')
    c.add_argument('--worker', required=True)
    c.add_argument('--sha256', required=True)
    c.add_argument('--library', action='append', default=[], metavar='SONAME=CANONICAL_PATH')
    c.add_argument('--out', type=Path, required=True)
    v = commands.add_parser('verify')
    v.add_argument('manifest', type=Path)
    args = parser.parse_args()
    if args.mode == 'verify':
        if args.manifest.stat().st_size > 1024 * 1024:
            raise ValueError('manifest size limit')
        result = verify(json.loads(args.manifest.read_text()))
    else:
        libraries = {}
        for item in args.library:
            name, path = item.split('=', 1)
            if name in libraries or not name or '/' in name:
                raise ValueError('duplicate/invalid library name')
            libraries[name] = path
        result = capture(args.worker, args.sha256, libraries)
        with args.out.open('x') as output:
            output.write(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    main()
