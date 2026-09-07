#!/usr/bin/env python3
"""Synchronize light-client templates in sibling workspace repositories."""

import argparse
import json
from pathlib import Path
import re
import sys


def main():
    fabric = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workspace", type=Path, default=fabric.parent)
    parser.add_argument("--check", action="store_true", help="Report drift without writing")
    args = parser.parse_args()
    targets = json.loads((fabric / "scripts/client-config-targets.json").read_text())
    canonical = (fabric / "crates/light-client/config/client.yml").read_text()
    pattern = re.compile(r"\$\{(client\.[^:}]+):(.*)\}")
    keys = {match[1] for match in pattern.finditer(canonical)}
    header = (
        "# Generated from light-fabric/crates/light-client/config/client.yml.\n"
        "# Update via light-fabric/scripts/sync-client-config.py; "
        "see its manifest for fallback overrides.\n"
    )
    errors = []
    pending = []
    skipped = set()
    checked = 0
    for relative, overrides in targets.items():
        repo = relative.split("/")[0]
        if not (args.workspace / repo).is_dir():
            skipped.add(repo)
            continue
        path = args.workspace / relative
        if not path.is_file():
            errors.append(f"Missing registered template: {relative}")
            continue
        if set(overrides) - keys:
            errors.append(f"Unknown override keys in {relative}: {set(overrides) - keys}")
            continue
        rendered = header + pattern.sub(
            lambda match: "${" + match[1] + ":" + overrides[match[1]] + "}"
            if match[1] in overrides else match[0], canonical
        )
        checked += 1
        if path.read_text() != rendered:
            pending.append((path, rendered))
            print(f"Drift: {relative}" if args.check else f"Sync: {relative}")

    # Catch new product templates that have not been enrolled in the manifest.
    for repo in sorted({name.split('/')[0] for name in targets}):
        directory = args.workspace / repo
        patterns = ["apps/*/config/client.yml"] if repo in {
            "light-fabric", "portal-service", "light-example-rs"
        } else ["*-rust/config/client.yml", "all-in-*/*-rust/config/client.yml"]
        for glob in patterns:
            for path in directory.glob(glob):
                relative = path.relative_to(args.workspace).as_posix()
                if relative not in targets:
                    errors.append(f"Unregistered template: {relative}")
    for error in errors:
        print(error, file=sys.stderr)
    if errors:
        return 1
    if not args.check:
        for path, rendered in pending:
            path.write_text(rendered)
    print(f"Checked {checked} templates; {len(pending)} {'out of sync' if args.check else 'updated'}.")
    if skipped:
        print("Repositories absent (not checked): " + ", ".join(sorted(skipped)))
    return int(args.check and bool(pending))


if __name__ == "__main__":
    sys.exit(main())
