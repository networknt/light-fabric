import importlib.util, json, os, shutil, stat, subprocess, sys, tempfile, unittest, zipfile
from pathlib import Path
APP=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('pack',APP/'scripts/package_baseline.py');pack=importlib.util.module_from_spec(spec);spec.loader.exec_module(pack)
class PackageFormats(unittest.TestCase):
    def setUp(self):self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup);self.root=Path(self.temp.name)
    def test_canonical_zip_is_byte_identical_and_has_no_extra_metadata(self):
        for name in ['one.zip','two.zip']:pack.canonical_zip(self.root/name,{'bootstrap':b'not-elf'},{'bootstrap':0o755},1760000000)
        self.assertEqual((self.root/'one.zip').read_bytes(),(self.root/'two.zip').read_bytes())
        with zipfile.ZipFile(self.root/'one.zip') as z:
            entry=z.getinfo('bootstrap');self.assertEqual(stat.S_IMODE(entry.external_attr>>16),0o755);self.assertEqual(entry.extra,b'');self.assertEqual(z.comment,b'')
    def test_baseline_rejects_helpers_wrong_mode_non_elf(self):
        for files,modes,expected in [({'bootstrap':b'x','worker':b'x'},{'bootstrap':0o755},'exactly bootstrap'),({'bootstrap':b'x'},{'bootstrap':0o644},'0755'),({'bootstrap':b'x'},{'bootstrap':0o755},'ELF')]:
            path=self.root/'package.zip';pack.canonical_zip(path,files,modes,1760000000)
            with self.assertRaisesRegex(ValueError,expected):pack.check_zip(path)
    def test_layer_zip_rejects_traversal_symlinks_and_unlisted_files(self):
        for name,mode in [('../escape',stat.S_IFREG|0o644),('/absolute',stat.S_IFREG|0o644),('symlink',stat.S_IFLNK|0o777),('a//b',stat.S_IFREG|0o644)]:
            path=self.root/'bad.zip'
            with zipfile.ZipFile(path,'w') as z:
                info=zipfile.ZipInfo(name);info.external_attr=mode<<16;z.writestr(info,b'x')
            with zipfile.ZipFile(path) as z:
                with self.assertRaises(ValueError):pack.safe_entries(z)
        path=self.root/'unlisted.zip';pack.canonical_zip(path,{'light-lambda-layer.json':json.dumps({'schema':'light-lambda-layer/v1','configDigest':'0'*64,'createdAt':'test-only','files':[]}).encode(),'unlisted':b'x'},{},1760000000)
        with self.assertRaisesRegex(ValueError,'contents differ'):pack.extract_layer(path,self.root/'extract')
    def test_profile_never_claims_worker_handlers_or_full_rie_coverage(self):
        profile=json.loads(pack.PROFILE.read_text());self.assertEqual(profile['workerDependencies'],[]);self.assertEqual(profile['rieEnabledHandlers'],['proxy']);self.assertIn('validator',profile['unavailableHandlers']);self.assertNotIn('validator',profile['supportedHandlers']);self.assertIn('router',profile['supportedHandlers']);self.assertIn('reject Init',profile['restrictions']['router'])
class RustLayerIntegration(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.bootstrap=Path(os.environ.get('LIGHT_LAMBDA_TEST_BOOTSTRAP',APP.parents[1]/'target/debug/bootstrap')).resolve()
        if not cls.bootstrap.is_file():raise RuntimeError('build actual bootstrap before layer integration tests')
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup);self.root=Path(self.temp.name);self.source=self.root/'source';self.source.mkdir()
        for name in ['handler.yml','values.yml']:shutil.copyfile(APP/'examples/local-baseline'/name,self.source/name)
    def test_real_offline_layer_determinism_tamper_and_input_preservation(self):
        before={p.name:p.read_bytes() for p in self.source.iterdir()}
        one=pack.layer(self.source,self.root/'one.zip',self.bootstrap);os.utime(self.source/'values.yml',(1,1));two=pack.layer(self.source,self.root/'two.zip',self.bootstrap)
        self.assertEqual(one['layer']['sha256'],two['layer']['sha256']);self.assertEqual(before,{p.name:p.read_bytes() for p in self.source.iterdir()});self.assertTrue(one['configDigest'])
        target=self.root/'opt';pack.extract_layer(self.root/'one.zip',target);(target/'values.yml').write_text((target/'values.yml').read_text()+'\n# tamper\n')
        with self.assertRaisesRegex(ValueError,'file digest mismatch'):pack.validate(self.bootstrap,target)
    def test_real_init_rejects_validator_and_router_rewriting(self):
        for alias,file,config,reason in [('validator','openapi-validator.yml',{'enabled':True},'enabled handler not supported'),('router','router.yml',{'enabled':True,'urlRewriteRules':['/x /new']},'Java rewrite execution is unavailable')]:
            with self.subTest(alias=alias):
                selected=self.root/alias;shutil.copytree(self.source,selected)
                cls='com.networknt.aws.lambda.handler.middleware.'+('validator.ValidatorMiddleware' if alias=='validator' else 'router.LambdaRouterMiddleware')
                (selected/'handler.yml').write_text(json.dumps({'handlers':[cls+'@'+alias,'com.networknt.aws.lambda.handler.middleware.proxy.LambdaProxyMiddleware@proxy'],'defaultHandlers':[alias,'proxy']}));(selected/file).write_text(json.dumps(config))
                output=self.root/(alias+'.zip')
                with self.assertRaisesRegex(ValueError,reason):pack.layer(selected,output,self.bootstrap)
                self.assertFalse(output.exists())
    def test_layer_source_symlinks_and_output_inside_input_are_rejected(self):
        (self.source/'link.yml').symlink_to(self.source/'values.yml')
        with self.assertRaisesRegex(ValueError,'symlinks'):pack.layer(self.source,self.root/'bad.zip',self.bootstrap)
        (self.source/'link.yml').unlink()
        with self.assertRaisesRegex(ValueError,'outside source'):pack.layer(self.source,self.source/'bad.zip',self.bootstrap)

    def test_logger_layer_requires_explicit_tooling_gate_not_ambient_permission(self):
        from unittest.mock import patch
        source=APP/'examples/logger-set-qualification'
        for name in ['handler.yml','logging.yml']:shutil.copyfile(source/name,self.source/name)
        (self.source/'values.yml').write_text(json.dumps({'light-lambda.stage':'layer-test',
            'logger.enabled':True,'security.enabled':True,'security.skipVerifyScopeWithoutSpec':True,
            'client.tokenKeyServerUrl':'http://127.0.0.1:19091','client.tokenKeyUri':'/jwks',
            'client.tokenKeyEnableHttp2':False}))
        with patch.dict(os.environ,{'LIGHT_LAMBDA_ALLOW_LOGGER_SET':'1',
                'LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS':'1','RUST_LOG':'info'}):
            with self.assertRaisesRegex(ValueError,'logger SET unsupported'):
                pack.validate(self.bootstrap,self.source)
            info=pack.layer(self.source,self.root/'logger.zip',self.bootstrap,allow_logger_set=True)
            self.assertTrue(info['configDigest'])
            with self.assertRaisesRegex(ValueError,'logger SET unsupported'):
                pack.validate(self.bootstrap,self.source)
if __name__=='__main__':unittest.main()
