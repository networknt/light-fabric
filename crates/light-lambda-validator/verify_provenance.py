#!/usr/bin/env python3
"""Verify retained upstream identities and explicit formatting derivations."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess


def sha(data):
    return hashlib.sha256(data).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--upstream-root', type=Path, action='append',
                        help='optional Cargo registry source directory for transformation replay')
    args = parser.parse_args()
    root = Path(__file__).resolve().parent
    provenance = json.loads((root / 'SOURCE-PROVENANCE.json').read_text())
    checked = replayed = 0
    for name, dependency in provenance['vendoredDependencies'].items():
        for row in dependency['files']:
            current = (root / 'vendor' / name / row['path']).read_bytes()
            if sha(current) != row['sha256']:
                raise ValueError(f'vendored content differs: {name}/{row["path"]}')
            checked += 1
            if row['identical'] and row['sha256'] != row['sourceSha256']:
                raise ValueError('inconsistent upstream identity')
            if not args.upstream_root:
                continue
            candidates = [p / Path(dependency['source']).name / row['path'] for p in args.upstream_root]
            source = next((p for p in candidates if p.is_file()), None)
            if source is None:
                raise ValueError(f'no explicit upstream input for {name}/{row["path"]}')
            original = source.read_bytes()
            if sha(original) != row['sourceSha256']:
                raise ValueError(f'upstream identity differs: {name}/{row["path"]}')
            transformation = row.get('transformation')
            if transformation:
                version = subprocess.check_output(['rustfmt', '--version'], text=True).strip()
                if version != transformation['tool']:
                    raise ValueError('formatting replay requires the recorded rustfmt version')
                formatted = subprocess.run(['rustfmt', '--edition', '2021', '--emit', 'stdout'],
                                           input=original, capture_output=True, check=True).stdout
                if formatted != current:
                    raise ValueError(f'formatting derivation differs: {name}/{row["path"]}')
                replayed += 1
    print(json.dumps({'filesVerified': checked, 'formattingDerivationsReplayed': replayed,
                      'upstreamVerified': args.upstream_root is not None}))


if __name__ == '__main__':
    main()
