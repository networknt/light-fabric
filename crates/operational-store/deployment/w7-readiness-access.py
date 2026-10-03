#!/usr/bin/env python3
"""Owner-run least-privilege sharing for a distinct readiness-container UID."""
import argparse
import fcntl
import grp
import os
from pathlib import Path
import stat
import sys


def arrange(root, uid, gid, check=False):
    owner = os.geteuid()
    if uid <= 0 or gid <= 0 or uid == owner or gid not in os.getgroups() + [os.getegid()]:
        raise ValueError('IDENTITY_OR_PRIVATE_GROUP')
    # Owner must review a dedicated group with no unrelated members. Never use
    # an unrestricted group such as nogroup as the production sharing boundary.
    group = grp.getgrgid(gid)
    if group.gr_name in {'nogroup', 'nobody', 'users', 'staff', 'docker'}:
        raise ValueError('PRIVATE_GROUP_REQUIRED')
    root = Path(root).resolve()
    state = root / '.runtime/w7'
    # State lives in a protected owner checkout. Every path is real, owner-owned;
    # don't follow a symlink into arbitrary credentials/state.
    targets = [(root / '.runtime', 0o710), (state, 0o710),
               (state / 'prepared.json', 0o640), (state / 'startup-ready.sha256', 0o640),
               (state / 'w7.lock', 0o660), (state / 'controller-page-authorization', 0o640)]
    for path, _mode in targets:
        kind_ok = path.is_dir() if path in {root / '.runtime', state} else path.is_file()
        if path.is_symlink() or not kind_ok or path.stat().st_uid != owner:
            raise ValueError('OWNER_PATH_REQUIRED')
    with (state / 'w7.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        for path, mode in targets:
            if check:
                if path.stat().st_gid != gid or stat.S_IMODE(path.stat().st_mode) != mode:
                    raise ValueError('ACCESS_NOT_PREPARED')
            else:
                os.chown(path, -1, gid)
                os.chmod(path, mode)
    return targets


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--assets', required=True)
    parser.add_argument('--uid', type=int, required=True)
    parser.add_argument('--gid', type=int, required=True)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    try:
        arrange(args.assets, args.uid, args.gid, args.check)
    except (OSError, ValueError, KeyError):
        print('W7_READINESS_ACCESS_REFUSED', file=sys.stderr)
        return 2
    print('W7_READINESS_ACCESS_VERIFIED' if args.check else 'W7_READINESS_ACCESS_PREPARED')
    return 0


if __name__ == '__main__':
    sys.exit(main())
