"""Check the standalone package and prove a missing packaged SQL is rejected."""
from pathlib import Path
import shutil
import subprocess
import tempfile

repository = Path(__file__).resolve().parent
validator = repository / "issue425-validate-bundle.py"
source = repository / "crates/operational-store/release/bundle"
with tempfile.TemporaryDirectory(prefix="issue425-package-") as directory:
    root = Path(directory) / "bundle"
    shutil.copytree(source, root)
    def check(command):
        return subprocess.run(command, cwd=root, capture_output=True, text=True)
    validator_command = ["rtk", "proxy", "python3", str(validator), "--bundle-root", str(root)]
    checksum_command = ["rtk", "proxy", "sha256sum", "--check", "bundle.sha256"]
    assert check(validator_command).returncode == 0
    assert check(checksum_command).returncode == 0
    migration = root / "crates/workflow-store/migrations/workflow-postgres/0020_artifact_legacy_retirement.sql"
    migration.unlink()  # Only the private temporary copy, never repository contents.
    assert check(validator_command).returncode != 0
    assert check(checksum_command).returncode != 0
print("Package regression: standalone contents pass; missing packaged 0020 rejected by both validators")
