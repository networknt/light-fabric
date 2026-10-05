"""Exact bundle and feature preparation regressions; no live adapters."""
import importlib.util
import json
from pathlib import Path
import shutil
import tempfile
import unittest
from bundle_contract import verify_bundle
from w7_rollout import Refusal, verify_assets, migration_needed
from w7_ownership import OwnershipContract, OwnershipError, verify_companion

HERE=Path(__file__).resolve().parent
ASSETS=HERE.parent/'release'

class BundleContractTests(unittest.TestCase):
    def test_current_reviewed_identity_and_source_bytes(self):
        version, rows=verify_bundle(ASSETS/'bundle')
        self.assertEqual((version,len(rows)),('2.7.0',45))
        self.assertEqual(rows,verify_assets(ASSETS)[1])
        verify_companion(ASSETS)
        fabric=HERE.parents[2]
        for row in rows:
            self.assertEqual((fabric/row[4]).read_bytes(),(ASSETS/'bundle'/row[4]).read_bytes())

    def test_missing_reordered_modified_mismatched_and_extra_refused(self):
        mutations={
            'missing':lambda root:(root/'bundle'/verify_bundle(root/'bundle')[1][-1][4]).unlink(),
            'reordered':lambda root:(root/'bundle/migration-order.tsv').write_text('\n'.join(reversed((root/'bundle/migration-order.tsv').read_text().splitlines()))+'\n'),
            'modified':lambda root:(root/'bundle'/verify_bundle(root/'bundle')[1][-1][4]).write_text('-- modified\n'),
            'mismatched':lambda root:(root/'bundle/manifest.json').write_text((root/'bundle/manifest.json').read_text().replace('2.7.0','2.8.0')),
            'extra':lambda root:(root/'bundle/unreviewed.sql').write_text('SELECT 1;'),
            'companion':lambda root:(root/'bin/bundle_contract.py').write_text('# modified\n'),
        }
        for name,mutate in mutations.items():
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temp:
                root=Path(temp)/'release';shutil.copytree(ASSETS,root,ignore=shutil.ignore_patterns('__pycache__'))
                mutate(root)
                with self.assertRaises((Refusal,OwnershipError,OSError,ValueError)):
                    verify_assets(root);verify_companion(root)

    def test_feature_is_optional_ordered_preparation_stage(self):
        rows=[('0024_workflow_expression_profile','unused','a','workflow-store','workflow_ops'),('0025_workflow_operation_receipts','unused','b','workflow-store','workflow_ops'),('0026_host_tool_workflow_access','unused','c','workflow-store','workflow_ops')]
        class Backend:
            root=ASSETS
            def migrations(self,gate):return rows
            def sql(self,gate,query):return json.dumps(self.recorded)
        backend=Backend(); contract=OwnershipContract(backend)
        records=[{'migration_owner':r[3],'schema_name':r[4],'migration_id':r[0],'migration_digest':'sha256:'+r[2]} for r in rows]
        for count in range(4):
            backend.recorded=records[:count]
            self.assertEqual(contract.stage({'kind':'operational'}),count)
        backend.recorded=[records[-1]]
        with self.assertRaises(OwnershipError):contract.stage({'kind':'operational'})
        self.assertTrue(migration_needed('','c','operational',rows[-1][0]))
        self.assertFalse(migration_needed('c','c','operational',rows[-1][0]))

if __name__=='__main__':unittest.main()
