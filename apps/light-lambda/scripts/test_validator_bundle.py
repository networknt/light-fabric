"""Integrity packaging tests only; worker-shaped bytes are never executable."""
import copy
import json
from pathlib import Path
import tempfile
import unittest
import zipfile

import validator_bundle as bundle


class BundleTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.inputs = {}
        files = {}
        for role, (name, mode) in bundle.ROLES.items():
            data = {'worker': b'non-executable worker fixture', 'source': b'openapi: 3.0.0\n', 'settings': b'enabled: true\n'}[role]
            path = self.root / role
            path.write_bytes(data)
            self.inputs[role] = path
            files[role] = dict(member=name, mode=mode, bytes=len(data), sha256=bundle.digest(data))
        self.value = dict(schema=bundle.SCHEMA, profile='local-test/v1',
                          target='linux-x86_64-static-worker', sourceFormat='yaml', files=files)
        self.catalogue = self.root / 'catalogue.json'
        self.save_catalogue()

    def save_catalogue(self):
        self.raw = (json.dumps(self.value, sort_keys=True) + '\n').encode()
        self.catalogue.write_bytes(self.raw)
        self.pin = bundle.digest(self.raw)

    def build(self, name='bundle.zip'):
        path = self.root / name
        result = bundle.build(self.catalogue, self.pin, self.inputs, path, 315532800)
        return path, result

    def test_reproducible_bytes_and_no_execution_claim(self):
        one, result = self.build()
        two, _ = self.build('second.zip')
        self.assertEqual(one.read_bytes(), two.read_bytes())
        self.assertEqual(result['filesVerified'], 3)
        self.assertFalse(result['workerExecuted'])
        self.assertFalse(result['runtimeAdmissionProved'])
        self.assertFalse(result['lambdaPackageQualified'])

    def test_catalogue_pin_cannot_be_relearned(self):
        with self.assertRaisesRegex(ValueError, 'catalogue digest'):
            bundle.catalogue(self.raw, '0' * 64)

    def test_each_changed_input_rejects_without_output(self):
        for role in bundle.ROLES:
            with self.subTest(role=role):
                path = self.inputs[role]
                original = path.read_bytes()
                path.write_bytes(bytes([original[0] ^ 1]) + original[1:])
                with self.assertRaisesRegex(ValueError, 'pinned content'):
                    self.build()
                self.assertFalse((self.root / 'bundle.zip').exists())
                path.write_bytes(original)

    def test_missing_input_rejects_without_output(self):
        self.inputs['source'].unlink()
        with self.assertRaises(FileNotFoundError):
            self.build()
        self.assertFalse((self.root / 'bundle.zip').exists())

    def test_symlink_input_rejects(self):
        path = self.inputs['source']
        path.rename(self.root / 'source-real')
        path.symlink_to(self.root / 'source-real')
        with self.assertRaises(OSError):
            self.build()

    def test_wrong_size_rejects(self):
        self.inputs['source'].write_bytes(b'x')
        with self.assertRaisesRegex(ValueError, 'pinned content'):
            self.build()

    def test_duplicate_manifest_json_keys_reject(self):
        raw = self.raw.replace(b'{', b'{"schema":"duplicate",', 1)
        with self.assertRaisesRegex(ValueError, 'duplicate catalogue'):
            bundle.catalogue(raw, bundle.digest(raw))

    def test_unknown_role_and_fields_reject(self):
        for location in ('root', 'roles', 'entry'):
            value = copy.deepcopy(self.value)
            target = {'root': value, 'roles': value['files'], 'entry': value['files']['source']}[location]
            target['unexpected'] = 1
            raw = json.dumps(value).encode()
            with self.assertRaises(ValueError):
                bundle.catalogue(raw, bundle.digest(raw))

    def test_path_mode_boolean_size_and_profile_reject(self):
        for field, wrong in [('member', '../escape'), ('mode', 0o777), ('bytes', True), ('sha256', 'bad')]:
            value = copy.deepcopy(self.value)
            value['files']['worker'][field] = wrong
            raw = json.dumps(value).encode()
            with self.assertRaises(ValueError):
                bundle.catalogue(raw, bundle.digest(raw))
        self.value['target'] = 'unreviewed-target'
        self.save_catalogue()
        with self.assertRaisesRegex(ValueError, 'catalogue profile'):
            self.build()

    def test_aggregate_limit_rejects_before_io(self):
        for row in self.value['files'].values():
            row['bytes'] = bundle.MAX_FILE
        self.save_catalogue()
        with self.assertRaisesRegex(ValueError, 'aggregate'):
            self.build()

    def test_existing_output_preserved(self):
        path, _ = self.build()
        original = path.read_bytes()
        with self.assertRaises(FileExistsError):
            self.build()
        self.assertEqual(path.read_bytes(), original)

    def test_modified_and_extra_archive_members_reject(self):
        path, _ = self.build()
        with zipfile.ZipFile(path) as original:
            entries = {info.filename: original.read(info) for info in original.infolist()}
        for mode in ('tamper', 'extra'):
            files = dict(entries)
            if mode == 'tamper':
                files['config/openapi.yaml'] = b'x' * len(files['config/openapi.yaml'])
            else:
                files['unexpected'] = b'extra'
            output = self.root / (mode + '.zip')
            modes = {bundle.MANIFEST: 0o644, **{name: bits for name, bits in bundle.ROLES.values()}}
            bundle.package_baseline.canonical_zip(output, files, modes, 315532800)
            with self.assertRaises(ValueError):
                bundle.verify(output, self.pin)

    def test_odd_epoch_rejects_before_output(self):
        with self.assertRaisesRegex(ValueError, 'ZIP epoch'):
            bundle.build(self.catalogue, self.pin, self.inputs, self.root / 'odd.zip', 315532801)

    def test_archive_mode_and_traversal_reject(self):
        path, _ = self.build()
        with zipfile.ZipFile(path) as original:
            files = {info.filename: original.read(info) for info in original.infolist()}
        modes = {bundle.MANIFEST: 0o644, **{name: bits for name, bits in bundle.ROLES.values()}}
        modes['worker/precision-worker'] = 0o777
        wrong_mode = self.root / 'wrong-mode.zip'
        bundle.package_baseline.canonical_zip(wrong_mode, files, modes, 315532800)
        with self.assertRaisesRegex(ValueError, 'size/mode'):
            bundle.verify(wrong_mode, self.pin)
        files['../escape'] = b'x'
        traversal = self.root / 'traversal.zip'
        bundle.package_baseline.canonical_zip(traversal, files, modes, 315532800)
        with self.assertRaisesRegex(ValueError, 'invalid or duplicate ZIP'):
            bundle.verify(traversal, self.pin)

    def test_duplicate_archive_members_reject(self):
        import warnings
        path, _ = self.build()
        with warnings.catch_warnings():
            warnings.simplefilter('ignore', UserWarning)
            with zipfile.ZipFile(path, 'a') as archive:
                archive.writestr('config/openapi.yaml', b'duplicate')
        with self.assertRaisesRegex(ValueError, 'invalid or duplicate ZIP'):
            bundle.verify(path, self.pin)

    def test_unpinned_catalogue_inside_archive_reject(self):
        path, _ = self.build()
        with zipfile.ZipFile(path) as original:
            files = {info.filename: original.read(info) for info in original.infolist()}
        value = copy.deepcopy(self.value)
        value['profile'] = 'other-profile/v1'
        files[bundle.MANIFEST] = json.dumps(value).encode()
        output = self.root / 'other-profile.zip'
        modes = {bundle.MANIFEST: 0o644, **{name: bits for name, bits in bundle.ROLES.values()}}
        bundle.package_baseline.canonical_zip(output, files, modes, 315532800)
        with self.assertRaisesRegex(ValueError, 'catalogue digest'):
            bundle.verify(output, self.pin)


if __name__ == '__main__':
    unittest.main()
