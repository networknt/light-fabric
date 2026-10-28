"""Structural regressions only; never substitute for real-child execution."""
import json
from pathlib import Path
import tempfile
import unittest
import yaml
import native_reconciled as n


class NativeReconciled(unittest.TestCase):
    def test_full_alias_closure_preserves_every_native_chain_and_admin_binding(self):
        with tempfile.TemporaryDirectory() as temp:
            full, qualification = n.prepare_candidates(Path(temp) / 'candidate')
            handler = json.loads((full / 'handler.yml').read_text())
            native = yaml.safe_load((n.NATIVE / 'handler.yml').read_text())
            for key in ['chains', 'paths', 'defaultHandlers']:
                self.assertEqual(handler[key], native[key])
            aliases = {entry.split('@')[-1] for entry in handler['handlers']}
            self.assertTrue({'requestCors', 'responseCors', 'cache', 'setLogger', 'validator'} <= aliases)
            for chain in handler['chains'].values():
                self.assertTrue(set(chain) <= aliases)
            for binding in handler['paths']:
                self.assertTrue(set(binding['exec']) <= aliases | handler['chains'].keys())
            functions = yaml.safe_load((n.NATIVE / 'values.yml').read_text())['lambda-proxy.functions']
            self.assertEqual(set(json.loads((full / 'lambda-proxy.yml').read_text())['functions']), set(functions))
            self.assertNotIn('/v1/pets/{petId}@put', functions)
            self.assertEqual((full / 'openapi.yml').read_bytes(), (n.NATIVE / 'openapi.yaml').read_bytes())
            for profile in [full, qualification]:
                validator = yaml.safe_load((profile / 'openapi-validator.yml').read_text())
                self.assertTrue(validator['enabled'])
                self.assertFalse(validator['skipBodyValidation'])
                self.assertEqual(validator['skipPathPrefixes'], [])
            self.assertEqual(json.loads((qualification / 'handler.yml').read_text())['defaultHandlers'],
                             ['specification', 'validator', 'proxy'])


if __name__ == '__main__':
    unittest.main()
