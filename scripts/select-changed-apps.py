#!/usr/bin/env python3
"""Select release images affected by uncommitted light-fabric files."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path


TARGET_PACKAGES = {
    "light-a2a": ("light-a2a",),
    "light-agent": ("light-agent",),
    "light-deployer": ("light-deployer",),
    "light-gateway": ("light-gateway",),
    "light-identity-issuer": ("light-identity-issuer-service",),
    "light-workflow": ("light-workflow",),
    "light-workflow-runner": ("light-workflow-runner", "light-agent-worker"),
    "light-knowledge": ("light-knowledge",),
    "light-knowledge-admin": ("light-knowledge-admin",),
    "light-knowledge-worker": ("light-knowledge-worker",),
}


def run_bytes(args: list[str], cwd: Path) -> bytes:
    return subprocess.run(args, cwd=cwd, check=True, stdout=subprocess.PIPE).stdout


def changed_paths(root: Path) -> set[Path]:
    tracked = run_bytes(["git", "diff", "--no-renames", "--name-only", "-z", "HEAD"], root)
    untracked = run_bytes(
        ["git", "ls-files", "--others", "--exclude-standard", "-z"], root
    )
    names = set(tracked.split(b"\0")) | set(untracked.split(b"\0"))
    return {root / os.fsdecode(name) for name in names if name}


def cargo_metadata(root: Path) -> dict:
    output = run_bytes(
        ["cargo", "metadata", "--locked", "--format-version", "1"], root
    )
    return json.loads(output)


def global_input(path: Path, root: Path) -> bool:
    relative = path.relative_to(root)
    parts = relative.parts
    if relative.name in {"Cargo.toml", "Cargo.lock", ".dockerignore"} and len(parts) == 1:
        return True
    if relative.name in {"rust-toolchain", "rust-toolchain.toml"} and len(parts) == 1:
        return True
    return bool(parts and parts[0] in {".cargo", "patches"})


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--only", action="append", default=[])
    args = parser.parse_args()

    root = args.root.resolve()
    dirty = changed_paths(root)
    if not dirty:
        print("No uncommitted files; no images selected.", file=sys.stderr)
        return 0

    try:
        metadata = cargo_metadata(root)
    except (subprocess.CalledProcessError, FileNotFoundError, json.JSONDecodeError) as exc:
        print(f"Unable to inspect Cargo workspace: {exc}", file=sys.stderr)
        return 2

    packages = metadata["packages"]
    by_id = {package["id"]: package for package in packages}
    resolve = metadata.get("resolve") or {}
    deps: dict[str, set[str]] = {
        node["id"]: {edge["pkg"] for edge in node.get("deps", [])}
        for node in resolve.get("nodes", [])
    }
    package_dirs = sorted(
        ((Path(package["manifest_path"]).parent.resolve(), package) for package in packages),
        key=lambda item: len(item[0].parts),
        reverse=True,
    )

    changed_package_ids: set[str] = set()
    global_change = False
    for path in sorted(dirty):
        if not path.is_relative_to(root):
            continue
        if global_input(path, root):
            global_change = True
            print(f"Changed build-wide input: {path.relative_to(root)}", file=sys.stderr)
            continue
        owner = next(
            (package for directory, package in package_dirs if path.is_relative_to(directory)),
            None,
        )
        if owner is not None:
            changed_package_ids.add(owner["id"])
            print(
                f"Changed package input: {path.relative_to(root)} -> {owner['name']}",
                file=sys.stderr,
            )
        else:
            print(f"Changed non-image input: {path.relative_to(root)}", file=sys.stderr)

    target_ids: dict[str, set[str]] = {}
    for image, names in TARGET_PACKAGES.items():
        target_ids[image] = {
            package["id"] for package in packages if package["name"] in names
        }
        missing = set(names) - {by_id[package_id]["name"] for package_id in target_ids[image]}
        if missing:
            print(f"Cargo package(s) missing for {image}: {', '.join(sorted(missing))}", file=sys.stderr)
            return 2

    affected: list[str] = []
    requested = set(args.only)
    unknown = requested - set(TARGET_PACKAGES)
    if unknown:
        print(f"Unknown image selection: {', '.join(sorted(unknown))}", file=sys.stderr)
        return 2

    for image, ids in target_ids.items():
        if requested and image not in requested:
            continue
        closure = set(ids)
        pending = list(ids)
        while pending:
            current = pending.pop()
            for dependency in deps.get(current, set()):
                if dependency not in closure:
                    closure.add(dependency)
                    pending.append(dependency)
        if global_change or closure.intersection(changed_package_ids):
            affected.append(image)
            reason = "workspace-wide build input" if global_change else "Cargo dependency"
            print(f"Selected image: {image} ({reason})", file=sys.stderr)

    if not affected:
        print("No selected images are affected by uncommitted files.", file=sys.stderr)
        return 0
    print("\n".join(affected))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
