#!/usr/bin/env python3
"""Data-only pinned qualification packaging. Never Init, execute, or validate instances."""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import stat
import zipfile

PROFILE = "validator-lambda-qualification-v1"
WORKER = (17482280, "de7b4e4ea11b81283049acd70637167cab81e02d98c259ce374926fd1487b3ea")
SOURCE = (17065, "012efdf3682d893f7c473738d158615f2f0a293d988765ef4676eb3307c46101")
HANDLER = "830f9c3a9a570e3b4cb246a15f291a02fcd64d1ef4b8b0fda3be0536c914fcb7"
POLICY = dict(parent_requested_bytes=268435456, worker_requested_bytes=536870912,
              worker_address_space_bytes=1073741824, lifetime_cpu_seconds=16,
              platform_bytes=1073741824, headroom_bytes=268435456)
MAX_FILE = 32 << 20
MAX_TOTAL = 32 << 20


class Rejected(ValueError):
    """Generic diagnostic deliberately excludes paths and captured contents."""


def reject():
    raise Rejected("qualification layer rejected")


def identity(data):
    return hashlib.sha256(data).hexdigest()


def pairs(items):
    result = {}
    for key, value in items:
        if key in result:
            reject()
        result[key] = value
    return result


def parse(data):
    try:
        value = json.loads(data, object_pairs_hook=pairs)
    except (ValueError, UnicodeError, RecursionError):
        reject()
    # Explicit qualification-data limits, independent of Python's recursion limit.
    pending, nodes = [(value, 0)], 0
    while pending:
        item, depth = pending.pop()
        nodes += 1
        if depth > 64 or nodes > 65536:
            reject()
        if isinstance(item, dict):
            pending.extend((child, depth + 1) for child in item.values())
        elif isinstance(item, list):
            pending.extend((child, depth + 1) for child in item)
    return value


def relative(name):
    if not isinstance(name, str) or not name or "\\" in name or "\x00" in name:
        reject()
    path = PurePosixPath(name)
    if path.is_absolute() or str(path) != name or any(p in ("", ".", "..") for p in name.split("/")):
        reject()
    return path


def capture(root, name, limit):
    """Walk beneath one directory fd, O_NOFOLLOW at every component; own bytes."""
    parts = relative(name).parts
    descriptors = []
    try:
        fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        descriptors.append(fd)
        for part in parts[:-1]:
            fd = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            descriptors.append(fd)
        fd = os.open(parts[-1], os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=fd)
        descriptors.append(fd)
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or not 0 < info.st_size <= limit:
            reject()
        with os.fdopen(os.dup(fd), "rb") as source:
            data = source.read(limit + 1)
        if len(data) != info.st_size or len(data) > limit:
            reject()
        return data
    except OSError:
        reject()
    finally:
        for fd in reversed(descriptors):
            os.close(fd)


def build_layer(root, manifest_name, expected_manifest):
    raw = capture(root, manifest_name, 65536)
    if identity(raw) != expected_manifest:
        reject()
    manifest = parse(raw)
    if not isinstance(manifest, dict) or set(manifest) != {"profile", "policy", "sourceIdentities", "files"}:
        reject()
    if manifest["profile"] != PROFILE or manifest["policy"] != POLICY:
        reject()
    origins = manifest["sourceIdentities"]
    if not isinstance(origins, dict) or not 1 <= len(origins) <= 128:
        reject()
    for name, pin in origins.items():
        relative(name)
        if not isinstance(pin, str) or len(pin) != 64 or any(c not in "0123456789abcdef" for c in pin):
            reject()
        if identity(capture(root, name, 1 << 20)) != pin:
            reject()
    entries = manifest["files"]
    if not isinstance(entries, list) or not 3 <= len(entries) <= 64:
        reject()
    owned, sources, total = {}, set(), 0
    for entry in entries:
        if not isinstance(entry, dict) or set(entry) != {"path", "source", "bytes", "sha256"}:
            reject()
        name, source = entry["path"], entry["source"]
        relative(name)
        relative(source)
        if name in owned or source in sources or not (name.startswith("config/") or name == "precision/precision-worker"):
            reject()
        if any(name.startswith(old + "/") or old.startswith(name + "/") for old in owned):
            reject()
        if name == "config/qualification-manifest.json" or type(entry["bytes"]) is not int:
            reject()
        limit = min(MAX_FILE, MAX_TOTAL - total)
        data = capture(root, source, limit)
        if len(data) != entry["bytes"] or identity(data) != entry["sha256"]:
            reject()
        owned[name] = data
        sources.add(source)
        total += len(data)
    for name, pin in [("precision/precision-worker", WORKER), ("config/openapi.yaml", SOURCE)]:
        if name not in owned or (len(owned[name]), identity(owned[name])) != pin:
            reject()
    handler = parse(owned.get("config/handler.yml", b"null"))
    if not isinstance(handler, dict):
        reject()
    canonical = json.dumps(handler, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()
    if identity(b"light-lambda/required-validator-profile/v1\0" + canonical) != HANDLER:
        reject()
    # No source paths or credentials in the generated receipt; data validation only.
    receipt = dict(profile=PROFILE, manifestSha256=identity(raw), dataOnly=True,
                   initAdmissionProved=False, workerExecuted=False,
                   files={name: dict(bytes=len(data), sha256=identity(data)) for name, data in sorted(owned.items())})
    owned["config/qualification-manifest.json"] = (json.dumps(receipt, sort_keys=True, separators=(",", ":")) + "\n").encode()
    output = io.BytesIO()
    with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_STORED) as archive:
        for name, data in sorted(owned.items()):
            info = zipfile.ZipInfo(name, (1980, 1, 1, 0, 0, 0))
            info.create_system = 3
            info.external_attr = (stat.S_IFREG | (0o555 if name.startswith("precision/") else 0o444)) << 16
            archive.writestr(info, data)
    return output.getvalue(), receipt


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--manifest", required=True, help="Relative to root; reviewed JSON pins")
    parser.add_argument("--manifest-sha256", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        data, _ = build_layer(args.root, args.manifest, args.manifest_sha256)
        with args.output.open("xb") as target:
            target.write(data)
    except (OSError, Rejected):
        parser.exit(1, "qualification layer rejected; existing outputs are never overwritten\n")
    print(json.dumps(dict(zipSha256=identity(data), bytes=len(data), dataOnly=True, initAdmissionProved=False)))


if __name__ == "__main__":
    main()
