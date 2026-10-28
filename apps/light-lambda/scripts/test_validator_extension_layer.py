"""Pure packaging tests with synthetic ELF bytes; actual artifacts checked separately."""
import hashlib,importlib.util,json,stat,tempfile,unittest,zipfile
from pathlib import Path
from unittest.mock import patch
HERE=Path(__file__).parent
spec=importlib.util.spec_from_file_location('extension_layer_tested',HERE/'build-validator-extension-layer.py');p=importlib.util.module_from_spec(spec);spec.loader.exec_module(p)
class Packaging(unittest.TestCase):
 def setUp(self):
  self.temp=tempfile.TemporaryDirectory();self.root=Path(self.temp.name);self.parent=self.root/'parent';self.helper=self.root/'helper';self.parent.write_bytes(b'UNIT synthetic parent');self.helper.write_bytes(b'UNIT synthetic extension')
 def tearDown(self):self.temp.cleanup()
 def build(self,name='package'):
  with patch.object(p,'validate_elf',return_value={'unitSyntheticBytesOnly':True}):return p.build(self.parent,self.helper,self.root/name)
 def test_repeated_assembly_is_identical_and_modes_manifest_roles_are_exact(self):
  a=self.build();b=self.build('second');self.assertEqual(a['layerSha256'],b['layerSha256']);self.assertEqual(a['parentZipSha256'],b['parentZipSha256'])
  entries=list(p.safe_entries(self.root/'package/extension-layer.zip'));self.assertEqual({n:m for n,_,m in entries},{p.EXTENSION:0o755,p.RECEIPT:0o644});self.assertFalse(a['nativeExecutionPerformed'])
  manifest=json.loads((self.root/'package/bridge-manifest.json').read_text());self.assertEqual(manifest['extension']['sha256'],hashlib.sha256(self.helper.read_bytes()).hexdigest());self.assertEqual(manifest['owner_requested_bytes']+manifest['extension_requested_bytes'],manifest['parent_side_envelope_bytes'])
 def test_composition_preserves_data_and_rejects_hash_or_collision(self):
  report=self.build();data=self.root/'data.zip';p.archive(data,[('config/handler.yml',b'unchanged',0o444),('precision/precision-worker',b'UNIT not executable',0o555)]);sha=hashlib.sha256(data.read_bytes()).hexdigest();layer=self.root/'package/extension-layer.zip'
  result=p.compose(data,sha,layer,report['layerSha256'],self.root/'opt');self.assertTrue(result['originalDataEntriesUnchanged']);self.assertEqual((self.root/'opt/config/handler.yml').read_bytes(),b'unchanged')
  with self.assertRaises(ValueError):p.compose(data,'0'*64,layer,report['layerSha256'],self.root/'bad')
  other=self.root/'collision.zip';p.archive(other,[(p.EXTENSION,b'UNIT duplicate',0o755)])
  with self.assertRaises(ValueError):p.compose(other,hashlib.sha256(other.read_bytes()).hexdigest(),layer,report['layerSha256'],self.root/'collision')
 def test_extra_wrong_mode_or_mutated_helper_is_rejected(self):
  self.build();entries=list(p.safe_entries(self.root/'package/extension-layer.zip'));data=self.root/'data.zip';p.archive(data,[('config/values.yml',b'{}',0o444)]);sha=hashlib.sha256(data.read_bytes()).hexdigest()
  for index,modified in enumerate([entries+[('extra',b'x',0o644)],[(n,b,0o644) for n,b,m in entries],[(n,b'wrong' if n==p.EXTENSION else b,m) for n,b,m in entries]]):
   path=self.root/f'bad-{index}.zip';p.archive(path,modified)
   with self.assertRaises(ValueError):p.compose(data,sha,path,hashlib.sha256(path.read_bytes()).hexdigest(),self.root/f'opt-{index}')
 def test_symlink_and_traversal_are_not_layer_members(self):
  for index,name in enumerate(['../escape','/absolute','x//y']):
   path=self.root/f'unsafe-{index}.zip';p.archive(path,[(name,b'x',0o644)])
   with self.assertRaises(ValueError):list(p.safe_entries(path))
  path=self.root/'symlink.zip'
  with zipfile.ZipFile(path,'w') as z:
   info=zipfile.ZipInfo('link');info.external_attr=(stat.S_IFLNK|0o777)<<16;z.writestr(info,'target')
  with self.assertRaises(ValueError):list(p.safe_entries(path))
 def test_input_symlink_identical_roles_and_output_reuse_refuse(self):
  link=self.root/'link';link.symlink_to(self.parent)
  with self.assertRaises(ValueError):p.identity(link)
  with patch.object(p,'validate_elf',return_value={}):
   with self.assertRaises(ValueError):p.build(self.parent,self.parent,self.root/'same')
  self.build()
  with self.assertRaises(FileExistsError):self.build()
if __name__=='__main__':unittest.main()
