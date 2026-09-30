from pathlib import Path
import hashlib
import json
import argparse

parser = argparse.ArgumentParser()
parser.add_argument("--bundle-root", type=Path, default=Path("crates/operational-store/release/bundle"))
root = parser.parse_args().bundle_root
entries = json.loads((root / "manifest.json").read_text())["orderedMigrations"]
rows = (root / "migration-order.tsv").read_text().splitlines()
assert len(entries) == len(rows) == 39
for index, (entry, row) in enumerate(zip(entries, rows), 1):
    assert entry["order"] == index
    assert hashlib.sha256((root / entry["path"]).read_bytes()).hexdigest() == entry["sha256"]
    assert row.split("\t") == [str(entry[k]) for k in ("order", "owner", "schema", "migrationId", "path", "sha256")]
for line in (root / "bundle.sha256").read_text().splitlines():
    expected, name = line.split("  ", 1)
    target = root / name
    assert hashlib.sha256(target.read_bytes()).hexdigest() == expected, name
print("Bundle validation: 39 migration digests, order/manifest agreement, all bundle checksums pass")
