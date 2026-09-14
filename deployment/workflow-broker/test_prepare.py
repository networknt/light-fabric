import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("prepare", Path(__file__).with_name("prepare.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class PreparationTest(unittest.TestCase):
    def test_endpoint_credentials_are_rejected_before_preparation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            profile = json.loads(Path(__file__).with_name("local-profile.json").read_text())
            profile["brokerBaseUrl"] = "https://name:private@localhost:7443"
            manifest = root / "profile.json"
            manifest.write_text(json.dumps(profile))
            with self.assertRaisesRegex(ValueError, "invalid HTTPS endpoint"):
                module.prepare(manifest, root / "unused", root / "output")
            self.assertFalse((root / "output").exists())

    def test_private_material_is_separated_and_replay_preserves_keys(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "input"
            source.mkdir()
            profile = json.loads(Path(__file__).with_name("local-profile.json").read_text())
            profile["legacyLongLivedAppKeys"] = [{"issuer":"test-issuer","kid":"test-app-only"}]
            subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
                "-keyout", str(source / "server.key"), "-out", str(source / "server.pem"),
                "-days", "1", "-subj", "/CN=qualification", "-addext", "subjectAltName=URI:" + profile["sanUri"]],
                check=True, capture_output=True)
            certificate = (source / "server.pem").read_bytes()
            key = (source / "server.key").read_bytes()
            for name in ("client-ca.pem", "issuer-ca.pem", "callback.pem"):
                (source / name).write_bytes(certificate)
            (source / "callback.key").write_bytes(key)
            (source / "client-identity.pem").write_bytes(certificate + key)
            (source / "database-url").write_text("postgresql://workflow_broker_runtime:qualification-only@localhost/workflow_credentials")
            manifest = root / "profile.json"
            manifest.write_text(json.dumps(profile))
            output = root / "prepared"
            module.prepare(manifest, source, output)
            settings = json.loads((output / "workflow-settings.json").read_text())
            self.assertEqual(settings["provider"]["clientId"], profile["clientId"])
            self.assertEqual(settings["legacyLongLivedAppKeys"], profile["legacyLongLivedAppKeys"])
            self.assertEqual(settings["callbackTls"]["address"], "0.0.0.0:8447")
            ring = (output / "workflow/keyring.json").read_bytes()
            with self.assertRaises(FileExistsError):
                module.prepare(manifest, source, output)
            self.assertEqual(ring, (output / "workflow/keyring.json").read_bytes())
            for name in ("manifest.json", "workflow-settings.json", "register.sql"):
                value = (output / name).read_bytes()
                self.assertNotIn(key, value)
                self.assertNotIn(b"qualification-only", value)
            self.assertFalse((output / "issuer/client-identity.pem").exists())
            self.assertFalse((output / "workflow/server.key").exists())
            for name in ("workflow/keyring.json", "workflow/database-url", "storage-bootstrap.sql"):
                self.assertEqual((output / name).stat().st_mode & 0o777, 0o600)
            if destination := __import__("os").environ.get("A1_PREPARE_TEST_OUTPUT"):
                # Explicit disposable qualification output for PostgreSQL gates.
                __import__("shutil").copytree(output, destination)


if __name__ == "__main__":
    unittest.main()
