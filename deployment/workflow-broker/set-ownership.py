#!/usr/bin/env python3
"""Set only the two private mount trees to the pinned images' UID:GID values."""
import argparse
import json
import os
from pathlib import Path
import stat

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("directory", type=Path)
parser.add_argument("--issuer-owner", required=True, help="UID:GID from the pinned issuer image")
parser.add_argument("--workflow-owner", required=True, help="UID:GID from the pinned Workflow image")
args = parser.parse_args()
owners = {"issuer": args.issuer_owner, "workflow": args.workflow_owner}
for component, owner in owners.items():
    uid, gid = map(int, owner.split(":"))
    if uid <= 0 or gid <= 0:
        raise SystemExit("the service identity must be non-root")
    root = args.directory / component
    paths = [root, *root.rglob("*")]
    for path in paths:
        mode = path.lstat().st_mode
        if not (stat.S_ISREG(mode) or stat.S_ISDIR(mode)):
            raise SystemExit("unexpected object in private mount")
    for path in paths:
        os.chown(path, uid, gid, follow_symlinks=False)
        os.chmod(path, 0o700 if path.is_dir() else 0o600, follow_symlinks=False)
manifest = args.directory / "manifest.json"
data = json.loads(manifest.read_text())
data["runtimeMountOwners"] = owners
manifest.write_text(json.dumps(data, indent=2) + "\n")
