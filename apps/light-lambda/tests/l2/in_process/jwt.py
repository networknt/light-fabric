#!/usr/bin/env python3
"""Local composed profile. Reuses the packaged Validator L2 harness; no AWS."""
import argparse,base64,datetime,hashlib,importlib.util,json,os,subprocess,sys,time,zipfile
from pathlib import Path
from cryptography.hazmat.primitives.asymmetric import rsa,padding
from cryptography.hazmat.primitives import hashes
ROOT=Path(__file__).resolve().parents[5]
PREVIOUS=Path(__file__).resolve().parent
spec=importlib.util.spec_from_file_location('previous',PREVIOUS/'validator.py');previous=importlib.util.module_from_spec(spec);spec.loader.exec_module(previous)
l2=previous.l2;pack=previous.pack
PIN=os.environ.get('LIGHT_LAMBDA_TEST_PACKAGE_SHA256','')
KID='validator-jwt-local-rsa'

def b64(raw):return base64.urlsafe_b64encode(raw).decode().rstrip('=')
def public_number(n):return b64(n.to_bytes((n.bit_length()+7)//8,'big'))
def signer():
    # Private key never serialized, saved, printed or sent to a dependency.
    private=rsa.generate_private_key(public_exponent=65537,key_size=2048)
    numbers=private.public_key().public_numbers()
    public={'keys':[{'kty':'RSA','alg':'RS256','use':'sig','kid':KID,'n':public_number(numbers.n),'e':public_number(numbers.e)}]}
    def token(subject,**override):
        claims={'sub':subject,'user_id':subject,'client_id':'synthetic-client','iss':'test-issuer','aud':'test-audience','exp':int(time.time())+3600,'scope':'pets.uad'}
        claims.update(override)
        content=(b64(json.dumps({'alg':'RS256','kid':KID},separators=(',',':')).encode())+'.'+b64(json.dumps(claims,separators=(',',':')).encode())).encode()
        return content.decode()+'.'+b64(private.sign(content,padding.PKCS1v15(),hashes.SHA256()))
    return token,public

def safe(value,tokens):
    raw=json.dumps(value)
    for value in tokens:
        if value:raw=raw.replace(value,'[REDACTED_SYNTHETIC_JWT]')
    return json.loads(raw)
class PrivateRie(l2.Rie):
    def __init__(self,*args,**kwargs):
        super().__init__(*args,**kwargs);self.tokens=[];self.fake_source=previous.L2/'profile_dependencies.py';self.detected_log_leak=False
    def logs(self):
        raw=super().logs()
        for token in self.tokens:
            if token and token in raw:self.detected_log_leak=True;raw=raw.replace(token,'[REDACTED_SYNTHETIC_JWT]')
        return raw

def configure(source):
    previous.configure(source,True)
    config=json.loads((source/'handler.yml').read_text())
    config['handlers'].append('com.networknt.aws.lambda.handler.middleware.security.JwtVerifyMiddleware@jwt')
    config['defaultHandlers']=['specification','jwt','validator','proxy']
    (source/'handler.yml').write_text(json.dumps(config,indent=2)+'\n')
    values=json.loads((source/'values.yml').read_text())
    values.update({'light-lambda.stage':'local-validator-jwt-composition','security.enabled':True,'unified-security.enabled':True,'security.issuer':'test-issuer','security.audience':['test-audience'],'security.skipVerifyScopeWithoutSpec':True,'unified-security.pathPrefixAuths':[{'prefix':'/','jwt':True}]})
    (source/'values.yml').write_text(json.dumps(values,indent=2)+'\n')

def event(case,token=None):
    raw=previous.event({'path':'/v1/pets','method':'POST','body':case['body']})
    if not case.get('includeTraceability',True):del raw['multiValueHeaders']['X-Traceability-Id']
    if token:raw['multiValueHeaders']['authorization']=['Bearer '+token]
    # Deliberate caller-supplied spoof must never become the verified identity.
    raw['requestContext']['authorizer']={'principalId':'untrusted-caller','claims':{'sub':'forged'}}
    return raw

def build_layer(task,out):
    source=out/'source';configure(source)
    env={k:v for k,v in os.environ.items() if not k.startswith(('AWS_','LIGHT_LAMBDA_'))}
    env.update(AWS_EC2_METADATA_DISABLED='true',LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS='1')
    check=subprocess.run([str(task/'bootstrap'),'validate-config','--config-dir',str(source)],env=env,capture_output=True,text=True,timeout=30)
    (out/'offline-validation.json').write_text(check.stdout);(out/'offline-validation.stderr.log').write_text(check.stderr)
    report=json.loads(check.stdout);assert check.returncode!=0 and not report['valid'];assert any('offline validation does not start an engine' in s for s in report['errors']),report
    with PrivateRie(task,source,out/'init-probe') as app:
        observed=l2.invoke(app,event({'body':'{}'}));(out/'init-probe-response.json').write_text(json.dumps(observed,indent=2)+'\n')
        assert observed['httpStatus']==200 and observed['response']['statusCode']==401,observed
        assert json.loads(observed['response']['body'])['code']=='ERR10002',observed
        state=app.state();assert state['invoke']==[] and state['jwksFetches']==0,state
        ready=previous.ready(app);assert ready.get('layer_identity') is None
        digest=ready['config_digest']
    files={f.name:f.read_bytes() for f in sorted(source.iterdir())}
    stamp=pack.epoch();manifest={'schema':'light-lambda-layer/v1','files':[{'path':name,'sha256':pack.digest(raw)} for name,raw in sorted(files.items())],'configDigest':digest,'createdAt':datetime.datetime.fromtimestamp(stamp,datetime.timezone.utc).isoformat().replace('+00:00','Z')}
    files['light-lambda-layer.json']=(json.dumps(manifest,sort_keys=True,separators=(',',':'))+'\n').encode()
    layer=out/'jwt-validator-layer.zip';repeat=out/'jwt-validator-layer-repeat.zip'
    pack.canonical_zip(layer,files,{},stamp);pack.canonical_zip(repeat,files,{},stamp)
    assert pack.digest(layer.read_bytes())==pack.digest(repeat.read_bytes())
    opt=out/'opt';pack.extract_layer(layer,opt)
    (out/'layer-receipt.json').write_text(json.dumps({'layer':pack.file_id(layer),'manifest':manifest,'originalSpecUnchanged':True,'dataOnly':True,'realInitDigest':digest,'offlineEngineActivation':False},indent=2)+'\n')
    return opt,manifest

def main():
    parser=argparse.ArgumentParser();parser.add_argument('--out',required=True,type=Path);args=parser.parse_args();out=args.out.resolve();out.mkdir(parents=True,exist_ok=False)
    cases=json.loads((Path(__file__).parent/'cases.json').read_text());assert len(cases)==22 and cases[-1]['expectedInvokeTotal']==11
    (out/'cases.json').write_text(json.dumps(cases,indent=2)+'\n')
    package=Path(os.environ.get('LIGHT_LAMBDA_TEST_PACKAGE',''))
    if len(PIN)!=64 or not package.is_file():raise SystemExit('set LIGHT_LAMBDA_TEST_PACKAGE and LIGHT_LAMBDA_TEST_PACKAGE_SHA256')
    assert pack.digest(package.read_bytes())==PIN
    (out/'package-check.json').write_text(json.dumps(pack.check_zip(package),indent=2)+'\n')
    images={name:json.loads(l2.docker('image','inspect',l2.IMAGES[name]).stdout)[0] for name in ['rie','fake']};(out/'images.json').write_text(json.dumps({'pins':l2.IMAGES,'actual':images},indent=2)+'\n')
    task=out/'var-task';task.mkdir()
    with zipfile.ZipFile(package) as z:(task/'bootstrap').write_bytes(z.read('bootstrap'))
    (task/'bootstrap').chmod(0o755);opt,manifest=build_layer(task,out)
    records=[];observations=[];accepted=[]
    with PrivateRie(task,opt,out/'composed') as app:
        issue,public=signer();good=issue('synthetic-A');signature=base64.urlsafe_b64decode(good.rsplit('.',1)[1]+'==');tampered=bytes([signature[0]^1])+signature[1:]
        credentials={'missing':None,'malformed':'not-a-jwt','a':good,'b':issue('synthetic-B'),'bad-signature':good.rsplit('.',1)[0]+'.'+b64(tampered),'wrong-issuer':issue('wrong-issuer',iss='wrong-issuer'),'expired':issue('expired',exp=int(time.time())-3600),'wrong-audience':issue('wrong-audience',aud='wrong-audience'),'insufficient-scope':issue('insufficient',scope='pets.read')}
        app.tokens=[v for v in credentials.values() if v]
        l2.http(app.fake_url+'/_configure',{'jwks':public})
        (out/'public-key-identity.json').write_text(json.dumps({'kid':KID,'algorithm':'RS256','jwksSha256':hashlib.sha256(json.dumps(public,sort_keys=True,separators=(',',':')).encode()).hexdigest(),'privateKeyPersisted':False,'tokensPersisted':False},indent=2)+'\n')
        for case in cases:
            observed=l2.invoke(app,event(case,credentials[case['credential']]))
            raw=json.dumps(observed);leaked=[t for t in app.tokens if t in raw]
            record={'case':case['id'],**observed,'responseTokenLeak':bool(leaked)};records.append(safe(record,app.tokens));(out/'requests.json').write_text(json.dumps(records,indent=2)+'\n')
            assert not leaked,{'case':case['id'],'failure':'token-in-response'}
            assert observed['httpStatus']==200,{'case':case['id'],'outerHttpStatus':observed['httpStatus']}
            response=observed['response'];assert response['statusCode']==case['status'],{'case':case['id'],'expectedStatus':case['status'],'actual':safe(response,app.tokens)}
            if case['code']:assert json.loads(response['body'])['code']==case['code'],{'case':case['id'],'actual':safe(response,app.tokens)}
            else:
                assert response['body']=='hello from business' and response['multiValueHeaders']['x-business']==['yes']
                accepted.append(case)
            state=app.state();observations.append({'case':case['id'],'invokeRequests':len(state['invoke']),'jwksRequests':state['jwksFetches']});(out/'dependency-counts.json').write_text(json.dumps(observations,indent=2)+'\n')
            assert len(state['invoke'])==case['expectedInvokeTotal'],{'case':case['id'],'invokes':len(state['invoke']),'expected':case['expectedInvokeTotal']}
            assert state['jwksFetches']==case['expectedJwksTotal'],{'case':case['id'],'jwks':state['jwksFetches'],'expected':case['expectedJwksTotal']}
            assert state['http']==[]
        calls=app.state()['invoke'];assert len(calls)==11 and len({c['requestId'] for c in calls})==11
        for case,call in zip(accepted,calls):
            assert call['principalId']==case['subject'] and call['subject']==case['subject'],{'case':case['id'],'principal':call['principalId'],'subject':call['subject']}
            assert call['path']=='/v1/pets' and call['method']=='POST' and call['body']==case['body'],{'case':case['id'],'failure':'forwarding-mismatch'}
            assert call['signed'] and call['logType']=='None'
            assert 'authorization' not in call['headers'] and 'authorization' not in call['multiValueHeaders']
        logs=app.logs();assert not app.detected_log_leak,'JWT appeared in gateway logs'
        ready=previous.ready(app);assert ready['config_digest']==manifest['configDigest']==ready['layer_identity']
        info=app.inspect();assert info['HostConfig']['ReadonlyRootfs'];assert all(not m['RW'] for m in info['Mounts'] if m['Destination'] in ['/var/task','/opt'])
        result={'allPassed':True,'packageSha256':PIN,'cases':22,'expectationPasses':22,'invokeRequests':len(calls),'jwksRequests':app.state()['jwksFetches'],'rejectionsWithoutDispatch':11,'snapshotReadyReports':1,'subjects':[c['principalId'] for c in calls],'readOnlyTaskLayer':True,'configDigest':manifest['configDigest'],'algorithm':'RS256','privateKeysAndTokensMemoryOnly':True,'tokenLeaks':False,'separateInitProbeRequests':1,'separateInitProbeInvokeRequests':0,'separateInitProbeJwksRequests':0,'awsQualified':False,'defaultActivationChanged':False}
        (out/'results.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result,indent=2))
    assert pack.digest(package.read_bytes())==PIN
if __name__=='__main__':main()
