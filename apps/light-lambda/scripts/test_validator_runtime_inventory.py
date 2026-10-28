"""Non-child regression tests for the offline ELF inventory; never runs ELF input."""
import copy
import hashlib
import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('inventory', Path(__file__).with_name('validator-runtime-inventory.py'))
inventory = importlib.util.module_from_spec(spec)
spec.loader.exec_module(inventory)


class InventoryTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        # Use a host ELF solely as data. Neither it nor the precision child runs.
        cls.elf = Path('/usr/bin/true').resolve(strict=True)
        cls.digest = hashlib.sha256(cls.elf.read_bytes()).hexdigest()
        cls.libraries = {
            'libc.so.6': Path('/lib/x86_64-linux-gnu/libc.so.6').resolve(strict=True),
            'ld-linux-x86-64.so.2': Path('/lib64/ld-linux-x86-64.so.2').resolve(strict=True),
        }
        cls.manifest = inventory.capture(cls.elf, cls.digest, cls.libraries)

    def test_actual_elf_closure_and_versions_verify_without_execution_claim(self):
        result = inventory.verify(self.manifest)
        self.assertEqual(result['filesVerified'], 3)
        self.assertFalse(result['workerExecuted'])
        self.assertFalse(result['immutableRuntimeLoadingProved'])
        self.assertFalse(result['productionActivationApproved'])

    def test_wrong_worker_pin_is_not_rebaselined(self):
        with self.assertRaisesRegex(ValueError, 'worker digest mismatch'):
            inventory.capture(self.elf, '0' * 64, self.libraries)

    def test_missing_transitive_loader_pin_rejects(self):
        m = copy.deepcopy(self.manifest)
        del m['libraries']['ld-linux-x86-64.so.2']
        with self.assertRaisesRegex(ValueError, 'missing interpreter pin'):
            inventory.check_closure(m)

    def test_missing_library_rejects(self):
        m = copy.deepcopy(self.manifest)
        del m['libraries']['libc.so.6']
        with self.assertRaisesRegex(ValueError, 'missing dependency pin'):
            inventory.check_closure(m)

    def test_symbol_version_mismatch_rejects(self):
        m = copy.deepcopy(self.manifest)
        m['libraries']['libc.so.6']['elf']['definedVersions'] = []
        with self.assertRaisesRegex(ValueError, 'unsatisfied symbol versions'):
            inventory.check_closure(m)

    def test_soname_mismatch_rejects(self):
        m = copy.deepcopy(self.manifest)
        m['libraries']['libc.so.6']['elf']['soname'] = 'unrelated.so'
        with self.assertRaisesRegex(ValueError, 'soname mismatch'):
            inventory.check_closure(m)

    def test_unused_pin_rejects(self):
        m = copy.deepcopy(self.manifest)
        m['libraries']['unused.so'] = copy.deepcopy(m['libraries']['libc.so.6'])
        m['libraries']['unused.so']['path'] = '/not-opened-unused.so'
        with self.assertRaisesRegex(ValueError, 'unused library pin'):
            inventory.check_closure(m)

    def test_duplicate_physical_path_rejects(self):
        m = copy.deepcopy(self.manifest)
        m['libraries']['libc.so.6']['path'] = m['worker']['path']
        with self.assertRaisesRegex(ValueError, 'duplicate file path'):
            inventory.check_closure(m)

    def test_custom_search_path_needs_disposition(self):
        m = copy.deepcopy(self.manifest)
        m['worker']['elf']['searchPaths'] = ['$ORIGIN']
        with self.assertRaisesRegex(ValueError, 'separate disposition'):
            inventory.check_closure(m)

    def test_symlink_input_is_not_accepted_as_canonical(self):
        with tempfile.TemporaryDirectory() as directory:
            link = Path(directory) / 'link'
            link.symlink_to(self.elf)
            with self.assertRaisesRegex(ValueError, 'canonical absolute path'):
                inventory.capture_file(link)

    def test_byte_replacement_fails_verification(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'elf-data'
            path.write_bytes(self.elf.read_bytes())
            m = inventory.capture(path, self.digest, self.libraries)
            raw = bytearray(path.read_bytes())
            raw[-1] ^= 1
            path.write_bytes(raw)
            with self.assertRaisesRegex(ValueError, 'pinned bytes or ELF metadata changed'):
                inventory.verify(m)

    def test_truncated_or_non_native_input_rejects(self):
        for raw in [b'', b'not executable' * 8, b'\x7fELF' + bytes(60)]:
            with self.assertRaisesRegex(ValueError, 'not ELF64'):
                inventory.elf_metadata(raw)


if __name__ == '__main__':
    unittest.main()
