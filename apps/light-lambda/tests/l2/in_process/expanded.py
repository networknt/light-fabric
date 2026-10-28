#!/usr/bin/env python3
"""Local error/envelope and independent JWKS profiles; no production changes/AWS."""
import argparse,base64,datetime,hashlib,http,importlib.util,json,os,subprocess,time,zipfile
from pathlib import Path
from cryptography.hazmat.primitives.asymmetric import rsa,padding
from cryptography.hazmat.primitives import hashes
HERE=Path(__file__).resolve().parent
PRIOR=HERE
spec=importlib.util.spec_from_file_location('prior',PRIOR/'jwt.py');prior=importlib.util.module_from_spec(spec);spec.loader.exec_module(prior)
l2=prior.l2;pack=prior.pack;previous=prior.previous
PIN=os.environ.get('LIGHT_LAMBDA_TEST_PACKAGE_SHA256','');PACKAGE=Path(os.environ.get('LIGHT_LAMBDA_TEST_PACKAGE',''))
PRIVATE=['SYNTHETIC-PRIVATE-DETAIL-9a04','SYNTHETIC-PRIVATE-JWKS-DETAIL-9a04']
class ObservedRie(prior.PrivateRie):
    def __init__(self,*args,**kwargs):
        super().__init__(*args,**kwargs);self.fake_source=HERE/'dependencies.py'

def signer(kid):
    private=rsa.generate_private_key(public_exponent=65537,key_size=2048)
    n=private.public_key().public_numbers();public={'keys':[{'kty':'RSA','alg':'RS256','use':'sig','kid':kid,'n':prior.public_number(n.n),'e':prior.public_number(n.e)}]}
    def issue(subject,jti,**overrides):
        claims={'sub':subject,'user_id':subject,'client_id':'synthetic-client','iss':'test-issuer','aud':'test-audience','scope':'pets.uad','exp':int(time.time())+3600,'jti':jti};claims.update(overrides)
        content=(prior.b64(json.dumps({'alg':'RS256','kid':kid},separators=(',',':')).encode())+'.'+prior.b64(json.dumps(claims,separators=(',',':')).encode())).encode()
        return content.decode()+'.'+prior.b64(private.sign(content,padding.PKCS1v15(),hashes.SHA256()))
    return issue,public

def prepare(out,refresh):
    assert pack.digest(PACKAGE.read_bytes())==PIN
    (out/'package-check.json').write_text(json.dumps(pack.check_zip(PACKAGE),indent=2)+'\n')
    task=out/'var-task';task.mkdir()
    with zipfile.ZipFile(PACKAGE) as archive:(task/'bootstrap').write_bytes(archive.read('bootstrap'))
    (task/'bootstrap').chmod(0o755)
    source=out/'source';prior.configure(source)
    if refresh:
        values=json.loads((source/'values.yml').read_text());values['security.jwksRefreshMinIntervalSecs']=0;(source/'values.yml').write_text(json.dumps(values,indent=2)+'\n')
    env={k:v for k,v in os.environ.items() if not k.startswith(('AWS_','LIGHT_LAMBDA_'))};env.update(AWS_EC2_METADATA_DISABLED='true',LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS='1')
    offline=subprocess.run([str(task/'bootstrap'),'validate-config','--config-dir',str(source)],env=env,capture_output=True,text=True,timeout=30)
    (out/'offline-validation.json').write_text(offline.stdout);(out/'offline-validation.stderr.log').write_text(offline.stderr)
    report=json.loads(offline.stdout);assert offline.returncode!=0 and not report['valid'];assert any('offline validation does not start an engine' in e for e in report['errors'])
    with ObservedRie(task,source,out/'init-probe') as app:
        observed=l2.invoke(app,prior.event({'body':'{}'}));(out/'init-probe-response.json').write_text(json.dumps(observed,indent=2)+'\n')
        assert observed['httpStatus']==200 and observed['response']['statusCode']==401 and json.loads(observed['response']['body'])['code']=='ERR10002'
        assert app.state()['invoke']==[] and app.state()['jwksFetches']==0
        digest=previous.ready(app)['config_digest']
    files={f.name:f.read_bytes() for f in sorted(source.iterdir())};stamp=pack.epoch()
    manifest={'schema':'light-lambda-layer/v1','files':[{'path':n,'sha256':pack.digest(raw)} for n,raw in sorted(files.items())],'configDigest':digest,'createdAt':datetime.datetime.fromtimestamp(stamp,datetime.timezone.utc).isoformat().replace('+00:00','Z')}
    files['light-lambda-layer.json']=(json.dumps(manifest,sort_keys=True,separators=(',',':'))+'\n').encode()
    layer=out/'config-layer.zip';pack.canonical_zip(layer,files,{},stamp);pack.canonical_zip(out/'config-layer-repeat.zip',files,{},stamp)
    assert pack.digest(layer.read_bytes())==pack.digest((out/'config-layer-repeat.zip').read_bytes())
    opt=out/'opt';pack.extract_layer(layer,opt)
    (out/'layer-receipt.json').write_text(json.dumps({'layer':pack.file_id(layer),'configDigest':digest,'refreshMinIntervalSecs':0 if refresh else 30,'fileIdentities':manifest['files'],'dataOnly':True},indent=2)+'\n')
    return task,opt,manifest

def validate_record(case,observed,state,expected_jwks,errors):
    if observed['httpStatus']!=200:errors.append({'case':case['id'],'field':'outerHttpStatus','expected':200,'actual':observed['httpStatus']})
    response=observed['response']
    if response['statusCode']!=case['status']:errors.append({'case':case['id'],'field':'applicationStatus','expected':case['status'],'actual':response['statusCode']})
    if case.get('code'):
        try:code=json.loads(response['body']).get('code')
        except (ValueError,TypeError):code=None
        if code!=case['code']:errors.append({'case':case['id'],'field':'applicationCode','expected':case['code'],'actual':code})
    if len(state['invoke'])!=case['expectedInvokeTotal']:errors.append({'case':case['id'],'field':'cumulativeInvoke','expected':case['expectedInvokeTotal'],'actual':len(state['invoke'])})
    if state['jwksFetches']!=expected_jwks:errors.append({'case':case['id'],'field':'cumulativeJwks','expected':expected_jwks,'actual':state['jwksFetches']})
    if any(value in json.dumps(observed) for value in PRIVATE):errors.append({'case':case['id'],'field':'private-backend-detail-leaked'})

def backend_profile(task,opt,manifest,out):
    cases=json.loads((HERE/'backend-cases.json').read_text());modes=json.loads((HERE/'modes.json').read_text());records=[];errors=[];sent=[]
    with ObservedRie(task,opt,out/'application') as app:
        issue,public=signer('backend-local-rsa');tokens={'synthetic-A':issue('synthetic-A','backend-a'),'synthetic-B':issue('synthetic-B','backend-b'),'scope':issue('synthetic-A','backend-scope',scope='pets.read')}
        app.tokens=list(tokens.values());l2.http(app.fake_url+'/_configure',{'jwks':public})
        for case in cases:
            l2.http(app.fake_url+'/_configure',{'mode':case['mode']})
            token=None if case.get('missingJwt') else tokens['scope'] if case.get('wrongScope') else tokens[case['subject']]
            event=prior.event(case,token)
            if case.get('invalidBase64'):event['body']='%%%';event['isBase64Encoded']=True
            if case.get('validBase64'):event['body']=base64.b64encode(case['body'].encode()).decode();event['isBase64Encoded']=True
            if case.get('invalidHeader'):event['multiValueHeaders']['bad header']=['bad']
            before=len(app.state()['invoke']);observed=l2.invoke(app,event);state=app.state()
            if any(t in json.dumps(observed) for t in app.tokens):errors.append({'case':case['id'],'field':'JWTInResponse'})
            record={'case':case['id'],**prior.safe(observed,app.tokens),'invokeTotal':len(state['invoke']),'jwksTotal':state['jwksFetches']};records.append(record);(out/'requests.json').write_text(json.dumps(records,indent=2)+'\n')
            validate_record(case,observed,state,1,errors)
            delta=len(state['invoke'])-before
            if delta!=case['expectInvokeDelta']:errors.append({'case':case['id'],'field':'dispatchDelta','expected':case['expectInvokeDelta'],'actual':delta})
            if case['expectInvokeDelta']:sent.append(case)
            if case.get('code') is None:
                response=observed['response'];expected=modes[case['mode']]['expected']
                for field in ['body','isBase64Encoded']:
                    if response.get(field)!=expected[field]:errors.append({'case':case['id'],'field':field,'expected':expected[field],'actual':response.get(field)})
                if response.get('multiValueHeaders',{})!=expected['headers']:errors.append({'case':case['id'],'field':'responseHeaders','expected':expected['headers'],'actual':response.get('multiValueHeaders')})
                if case['mode']=='status599' and response.get('statusDescription')!='599 Unknown':errors.append({'case':case['id'],'field':'statusDescription','actual':response.get('statusDescription')})
            (out/'failures.json').write_text(json.dumps(errors,indent=2)+'\n')
        state=app.state();assert len(state['invoke'])==len(state['backendModes'])
        if len(state['invoke'])!=90:errors.append({'field':'finalInvokeTotal','expected':90,'actual':len(state['invoke'])})
        for case,call,mode in zip(sent,state['invoke'],state['backendModes']):
            if call['principalId']!=case['subject'] or call['subject']!=case['subject']:errors.append({'case':case['id'],'field':'verifiedIdentity','actual':call['principalId']})
            expected_body=base64.b64encode(case['body'].encode()).decode() if case.get('validBase64') else case['body']
            if call['body']!=expected_body or call['path']!='/v1/pets' or call['method']!='POST':errors.append({'case':case['id'],'field':'forwardingMismatch'})
            if mode['mode']!=case['mode'] or mode['base64']!=bool(case.get('validBase64')) or not mode['completed']:errors.append({'case':case['id'],'field':'receivedModeOrCompletionMismatch','actual':mode})
            if not mode['sdkPath'].endswith('/functions/test-only-business/invocations') or not call['signed'] or call['logType']!='None':errors.append({'case':case['id'],'field':'targetOrInvocationOptions'})
        if len({r['requestId'] for r in state['invoke']})!=90:errors.append({'field':'duplicateDispatchRequestId'})
        finish(app,manifest,out,errors,{'profile':'backend','requests':len(cases),'modes':len(modes),'invokeTransportAttempts':len(state['invoke']),'localHttpCompletions':sum(r['completed'] for r in state['backendModes']),'jwksRequests':state['jwksFetches'],'zeroDispatchControls':5,'responseExpectationPasses':len(cases)-len({e.get('case') for e in errors if e.get('case')})})

def jwks_profile(task,opt,manifest,out):
    cases=json.loads((HERE/'jwks-cases.json').read_text());keys={k:signer('key-E' if k=='E2' else 'key-'+k) for k in ['A','B','C','D','E','E2','Z']};records=[];errors=[];accepted=[]
    with ObservedRie(task,opt,out/'application') as app:
        for index,case in enumerate(cases):
            publish=case['publish']
            if publish:
                config={'jwksMode':publish if publish in ['503','malformed','empty'] else 'valid'}
                if publish in keys:config['jwks']=keys[publish][1]
                l2.http(app.fake_url+'/_configure',config)
            issue=keys[case['key']][0];token=issue('synthetic-'+case['key'],'fresh-'+str(index),**({'iss':'wrong-issuer'} if case['id']=='known-key-wrong-issuer' else {}))
            if case['id']=='known-key-invalid-signature':
                signature=base64.urlsafe_b64decode(token.rsplit('.',1)[1]+'==');token=token.rsplit('.',1)[0]+'.'+prior.b64(bytes([signature[0]^1])+signature[1:])
            app.tokens.append(token);body='{"name":"KeyPet"}'
            observed=l2.invoke(app,prior.event({'body':body},token));state=app.state()
            if any(t in json.dumps(observed) for t in app.tokens):errors.append({'case':case['id'],'field':'JWTInResponse'})
            records.append({'case':case['id'],**prior.safe(observed,app.tokens),'invokeTotal':len(state['invoke']),'jwksTotal':state['jwksFetches']});(out/'requests.json').write_text(json.dumps(records,indent=2)+'\n')
            validate_record(case,observed,state,case['expectedJwksTotal'],errors)
            if case['status']==200:accepted.append(case)
            (out/'failures.json').write_text(json.dumps(errors,indent=2)+'\n')
        state=app.state()
        for case,call in zip(accepted,state['invoke']):
            if call['subject']!='synthetic-'+case['key'] or call['principalId']!='synthetic-'+case['key'] or call['body']!='{"name":"KeyPet"}':errors.append({'case':case['id'],'field':'verifiedRecoveryIdentityBody'})
        if len(state['invoke'])!=16 or len({r['requestId'] for r in state['invoke']})!=16:errors.append({'field':'finalInvokeCountOrDuplicate'})
        if state['jwksFetches']!=11 or len(state['jwksEvents'])!=11 or not all(r['completed'] for r in state['jwksEvents']):errors.append({'field':'finalJwksCountOrCompletion'})
        if len(app.tokens)!=len(set(app.tokens)):errors.append({'field':'freshTokensNotUnique'})
        finish(app,manifest,out,errors,{'profile':'jwks','requests':22,'invokeTransportAttempts':len(state['invoke']),'localHttpCompletions':sum(r['completed'] for r in state['backendModes']),'jwksRequests':state['jwksFetches'],'jwksHttpCompletions':sum(r['completed'] for r in state['jwksEvents']),'responseExpectationPasses':22-len({e.get('case') for e in errors if e.get('case')}),'freshUniqueTokens':True,'refreshMinIntervalSecs':0,'productionDefaultIntervalUnchanged':30})

def finish(app,manifest,out,errors,result):
    logs=app.logs()
    if app.detected_log_leak:errors.append({'field':'JWTLogged'})
    if any(marker in logs for marker in PRIVATE):errors.append({'field':'privateDependencyDetailsLogged'})
    ready=previous.ready(app)
    if ready['config_digest']!=manifest['configDigest'] or ready['layer_identity']!=manifest['configDigest']:errors.append({'field':'snapshotLayerDigestMismatch'})
    info=app.inspect()
    if not info['HostConfig']['ReadonlyRootfs'] or any(m['RW'] for m in info['Mounts'] if m['Destination'] in ['/var/task','/opt']):errors.append({'field':'readOnlyMountMismatch'})
    result.update(allPassed=not errors,failures=errors,packageSha256=PIN,snapshotReadyReports=1,configDigest=manifest['configDigest'],privateKeysTokensMemoryOnly=True,rawSdkWireMeasured=False,awsExecuted=False,separateInitProbeRequests=1,separateInitProbeInvokes=0,separateInitProbeJwks=0)
    (out/'failures.json').write_text(json.dumps(errors,indent=2)+'\n');(out/'results.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result,indent=2),flush=True)

def main():
    parser=argparse.ArgumentParser();parser.add_argument('--profile',required=True,choices=['backend','jwks']);parser.add_argument('--out',required=True,type=Path);args=parser.parse_args();out=args.out.resolve();out.mkdir(parents=True,exist_ok=False)
    if len(PIN)!=64 or not PACKAGE.is_file():raise SystemExit('set LIGHT_LAMBDA_TEST_PACKAGE and LIGHT_LAMBDA_TEST_PACKAGE_SHA256 to the explicit qualified package')
    images={name:json.loads(l2.docker('image','inspect',l2.IMAGES[name]).stdout)[0] for name in ['rie','fake']};(out/'images.json').write_text(json.dumps({'pins':l2.IMAGES,'actual':images},indent=2)+'\n')
    task,opt,manifest=prepare(out,args.profile=='jwks')
    (backend_profile if args.profile=='backend' else jwks_profile)(task,opt,manifest,out)
    assert pack.digest(PACKAGE.read_bytes())==PIN
    if not json.loads((out/'results.json').read_text())['allPassed']:raise SystemExit(1)
if __name__=='__main__':main()
