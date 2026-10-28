"""Non-executing packaging regressions. Synthetic worker/source pins are unit-only."""
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import zipfile

spec = importlib.util.spec_from_file_location("qualification_layer", Path(__file__).with_name("build-validator-qualification-layer.py"))
layer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(layer)


class LayerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        handler = dict(defaultHandlers=["spec", "validator", "proxy"], handlers=[
            "com.networknt.aws.lambda.handler.middleware.specification.OpenApiMiddleware@spec",
            "com.networknt.aws.lambda.handler.middleware.validator.ValidatorMiddleware@validator",
            "com.networknt.aws.lambda.handler.middleware.proxy.LambdaProxyMiddleware@proxy"])
        values = {"worker": b"synthetic-unit-only-worker", "openapi": b"synthetic-unit-only-schema", "handler": json.dumps(handler).encode()}
        for name, data in values.items():
            (self.root / name).write_bytes(data)
        self.manifest = dict(profile=layer.PROFILE, policy=dict(layer.POLICY), sourceIdentities={"openapi": layer.identity(values["openapi"])}, files=[
            dict(path=dest, source=name, bytes=len(values[name]), sha256=layer.identity(values[name]))
            for name, dest in [("worker", "precision/precision-worker"), ("openapi", "config/openapi.yaml"), ("handler", "config/handler.yml")]])
        for name, key in [("worker", "WORKER"), ("openapi", "SOURCE")]:
            p = patch.object(layer, key, (len(values[name]), layer.identity(values[name])))
            p.start()
            self.addCleanup(p.stop)

    def build(self):
        data = json.dumps(self.manifest).encode()
        (self.root / "manifest.json").write_bytes(data)
        with patch("subprocess.Popen", side_effect=AssertionError("no child allowed")), patch("os.system", side_effect=AssertionError("no shell allowed")):
            return layer.build_layer(self.root, "manifest.json", layer.identity(data))

    def test_deterministic_owned_data_only_zip_and_modes(self):
        first, receipt = self.build()
        second, _ = self.build()
        self.assertEqual(first, second)
        self.assertFalse(receipt["initAdmissionProved"])
        self.assertFalse(receipt["workerExecuted"])
        with zipfile.ZipFile(io.BytesIO(first)) as archive:
            self.assertEqual(archive.namelist(), sorted(archive.namelist()))
            self.assertEqual(archive.getinfo("precision/precision-worker").external_attr >> 16 & 0o777, 0o555)

    def test_manifest_identity_cannot_be_self_authorized(self):
        self.build()
        with self.assertRaises(layer.Rejected):
            layer.build_layer(self.root, "manifest.json", "0" * 64)

    def test_missing_null_and_wrong_profile(self):
        for value in [None, {}, dict(self.manifest, profile="ordinary"), dict(self.manifest, files=None)]:
            with self.subTest(value=value):
                original = self.manifest
                self.manifest = value
                with self.assertRaises(layer.Rejected):
                    self.build()
                self.manifest = original

    def test_tampered_worker_source_provenance_and_handler(self):
        for name in ["worker", "openapi", "handler"]:
            original = (self.root / name).read_bytes()
            (self.root / name).write_bytes(b"tampered")
            with self.subTest(name=name), self.assertRaises(layer.Rejected):
                self.build()
            (self.root / name).write_bytes(original)

    def test_unapproved_policy_and_absent_required_resource(self):
        self.manifest["policy"]["lifetime_cpu_seconds"] += 1
        with self.assertRaises(layer.Rejected):
            self.build()
        self.manifest["policy"] = dict(layer.POLICY)
        self.manifest["files"].pop()
        with self.assertRaises(layer.Rejected):
            self.build()

    def test_duplicate_paths_and_traversal(self):
        for name in ["../outside", "/absolute", "config/../x", "config//x", "config/handler.yml", "config/openapi.yaml/descendant"]:
            original = self.manifest["files"][0]["path"]
            self.manifest["files"][0]["path"] = name
            with self.subTest(name=name), self.assertRaises(layer.Rejected):
                self.build()
            self.manifest["files"][0]["path"] = original

    def test_symlink_source_and_parent_and_manifest(self):
        (self.root / "link").symlink_to(self.root / "worker")
        self.manifest["files"][0]["source"] = "link"
        with self.assertRaises(layer.Rejected):
            self.build()
        (self.root / "dirlink").symlink_to(self.root, target_is_directory=True)
        self.manifest["files"][0]["source"] = "dirlink/worker"
        with self.assertRaises(layer.Rejected):
            self.build()
        (self.root / "manifest-link").symlink_to(self.root / "openapi")
        with self.assertRaises(layer.Rejected):
            layer.build_layer(self.root, "manifest-link", "0" * 64)

    def test_duplicate_manifest_keys_and_oversized_capture(self):
        with self.assertRaises(layer.Rejected):
            layer.parse(b'{"profile":null,"profile":null}')
        with self.assertRaises(layer.Rejected):
            layer.capture(self.root, "worker", 1)

    def test_rehashed_manifest_cannot_authorize_a_new_worker_or_handler(self):
        for name in ["worker", "handler"]:
            with self.subTest(name=name):
                data = (self.root / name).read_bytes()
                index = 0 if name == "worker" else 2
                entry = dict(self.manifest["files"][index])
                replacement = b"different worker" if name == "worker" else b'{"handlers":[],"defaultHandlers":[]}'
                (self.root / name).write_bytes(replacement)
                self.manifest["files"][index].update(bytes=len(replacement), sha256=layer.identity(replacement))
                with self.assertRaises(layer.Rejected):
                    self.build()
                self.manifest["files"][index] = entry
                (self.root / name).write_bytes(data)

    def test_source_identity_is_checked_and_capture_boundary_is_exact(self):
        self.manifest["sourceIdentities"]["openapi"] = "0" * 64
        with self.assertRaises(layer.Rejected):
            self.build()
        data = (self.root / "worker").read_bytes()
        self.assertEqual(layer.capture(self.root, "worker", len(data)), data)
        with self.assertRaises(layer.Rejected):
            layer.capture(self.root, "worker", len(data) - 1)

    def test_deep_json_is_a_generic_rejection(self):
        with self.assertRaisesRegex(layer.Rejected, "^qualification layer rejected$"):
            layer.parse(b"[" * 2000 + b"]" * 2000)


if __name__ == "__main__":
    unittest.main()
