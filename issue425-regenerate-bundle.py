"""Regenerate the standalone package from manifest-pinned canonical SQL."""
from pathlib import Path
import hashlib
import json
import shutil

root = Path("crates/operational-store/release/bundle")
manifest = json.loads((root / "manifest.json").read_text())
for entry in manifest["orderedMigrations"]:
    source = Path(entry["path"])
    assert hashlib.sha256(source.read_bytes()).hexdigest() == entry["sha256"], source
    target = root / entry["path"]
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, target)
(root / "migration-order.tsv").write_text("".join(
    "\t".join(str(entry[key]) for key in ("order", "owner", "schema", "migrationId", "path", "sha256")) + "\n"
    for entry in manifest["orderedMigrations"]
))
paths = ["manifest.json", "migration-order.tsv"] + [entry["path"] for entry in manifest["orderedMigrations"]]
(root / "bundle.sha256").write_text("".join(
    hashlib.sha256((root / path).read_bytes()).hexdigest() + "  " + path + "\n" for path in paths
))
print("Regenerated self-contained bundle: 39 canonical SQL files")
