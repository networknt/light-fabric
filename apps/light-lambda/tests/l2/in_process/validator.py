#!/usr/bin/env python3
"""Local in-process ZIP/layer qualification using the existing L2 harness only."""
import argparse,copy,datetime,hashlib,importlib.util,json,os,shutil,zipfile
from pathlib import Path
ROOT=Path(__file__).resolve().parents[5]
L2=ROOT/'apps/light-lambda/tests/l2'
spec=importlib.util.spec_from_file_location('l2',L2/'run.py');l2=importlib.util.module_from_spec(spec);spec.loader.exec_module(l2)
pack=l2.pack

def identity(path):return pack.file_id(path)
def ready(instance):
    rows=[]
    for line in instance.logs().splitlines():
        try:obj=json.loads(line)
        except ValueError:continue
        if obj.get('fields',{}).get('message')=='immutable snapshot ready':rows.append(obj['fields'])
    assert len(rows)==1,rows
    return rows[0]
def event(case):
    headers={'Content-Type':[case.get('contentType','application/json')], 'X-Traceability-Id':['00000000-0000-4000-8000-000000000001']}
    if 'requiredHeader' in case:headers['X-Required']=[case['requiredHeader']]
    return {'path':case['path'],'httpMethod':case['method'],'multiValueHeaders':headers,
        'multiValueQueryStringParameters':{'limit':[case['queryLimit']]} if 'queryLimit' in case else {},
        'body':case.get('body'),'isBase64Encoded':False,
        'requestContext':{'elb':{'targetGroupArn':'owned-local-validator-only'}}}
def instance(task,opt,out):
    obj=l2.Rie(task,opt,out);obj.fake_source=L2/'profile_dependencies.py';return obj

def configure(path,petstore):
    path.mkdir()
    sample=ROOT/'apps/light-lambda/migrations/native-petstore'
    for name in ['handler.yml','openapi-handler.yml','openapi-validator.yml']:shutil.copyfile(sample/name,path/name)
    source=sample/'openapi.yaml' if petstore else ROOT/'apps/light-lambda/tests/l1/data/validator-activation-openapi.json'
    shutil.copyfile(source,path/('openapi.yaml' if petstore else 'openapi.json'))
    functions={'/v1/pets@post':'test-only-business'} if petstore else {'/v1/decimal@post':'test-only-business','/v1/integer@post':'test-only-business','/v1/pets/{petId}@get':'test-only-business'}
    (path/'values.yml').write_text(json.dumps({'light-lambda.stage':'local-validator-package','lambda-proxy.region':'us-east-1','lambda-proxy.metricsInjection':False,'security.enabled':False,'unified-security.enabled':False,'client.tokenKeyServerUrl':'http://127.0.0.1:19091','client.tokenKeyUri':'/jwks','client.tokenKeyEnableHttp2':False}))
    (path/'lambda-proxy.yml').write_text(json.dumps({'enabled':True,'region':'us-east-1','endpointOverride':'http://127.0.0.1:19091','functions':functions,'metricsInjection':False}))
    return {'source':identity(source),'configuration':{f.name:identity(f) for f in sorted(path.iterdir())}}

def make_layer(task,out,petstore,stamp):
    name='petstore' if petstore else 'semantics'
    source=out/(name+'-source');origins=configure(source,petstore)
    # Do not pretend offline validation activated a precision engine. Its
    # fail-closed report is retained; real Init below establishes the digest.
    env={k:v for k,v in os.environ.items() if not k.startswith(('AWS_','LIGHT_LAMBDA_'))}
    env['AWS_EC2_METADATA_DISABLED']='true';env['LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS']='1'
    import subprocess
    p=subprocess.run([str(task/'bootstrap'),'validate-config','--config-dir',str(source)],env=env,cwd=out,capture_output=True,text=True,timeout=30)
    (out/(name+'-offline-validation.stdout.json')).write_text(p.stdout)
    (out/(name+'-offline-validation.stderr.log')).write_text(p.stderr)
    report=json.loads(p.stdout);assert p.returncode!=0 and report['valid'] is False
    assert any('offline validation does not start an engine' in error for error in report['errors']),report
    probe={'path':'/v1/pets' if petstore else '/v1/decimal','method':'POST','body':'{}'}
    with instance(task,source,out/(name+'-init-probe')) as app:
        observed=l2.invoke(app,event(probe));(out/(name+'-init-probe-response.json')).write_text(json.dumps(observed,indent=2)+'\n')
        assert observed['httpStatus']==200 and observed['response']['statusCode']==400,observed
        state=app.state();assert state['invoke']==[] and state['jwksFetches']==0,state
        receipt=ready(app);assert receipt.get('layer_identity') is None,receipt
        digest=receipt['config_digest']
    files={f.name:f.read_bytes() for f in sorted(source.iterdir())}
    manifest={'schema':'light-lambda-layer/v1','files':[{'path':name,'sha256':pack.digest(raw)} for name,raw in sorted(files.items())],'configDigest':digest,'createdAt':datetime.datetime.fromtimestamp(stamp,datetime.timezone.utc).isoformat().replace('+00:00','Z')}
    files['light-lambda-layer.json']=(json.dumps(manifest,sort_keys=True,separators=(',',':'))+'\n').encode()
    layer=out/(name+'-layer.zip');repeat=out/(name+'-layer-repeat.zip')
    pack.canonical_zip(layer,files,{},stamp);pack.canonical_zip(repeat,files,{},stamp)
    assert pack.digest(layer.read_bytes())==pack.digest(repeat.read_bytes())
    opt=out/(name+'-opt');pack.extract_layer(layer,opt)
    (out/(name+'-layer-receipt.json')).write_text(json.dumps({'layer':identity(layer),'configDigest':digest,'files':manifest['files'],'origins':origins,'offlineValidation':'expected fail-closed','runtimeQualification':'see separately counted final profiles','dataOnly':True,'containsWorker':False},indent=2)+'\n')
    return opt,manifest

def run_profile(task,opt,manifest,cases,out):
    records=[];accepted=[]
    with instance(task,opt,out) as app:
        for case in cases:
            record=l2.invoke(app,event(case));records.append({'case':case['id'],**record})
            (out/'requests.json').parent.mkdir(parents=True,exist_ok=True)
            (out/'requests.json').write_text(json.dumps(records,indent=2)+'\n')
            assert record['httpStatus']==200,record
            response=record['response'];assert response['statusCode']==case['status'],record
            if case['status']==200:
                accepted.append(case);assert response['body']=='hello from business',record
                assert response['multiValueHeaders']['x-business']==['yes'],record
            else:
                body=json.loads(response['body']);assert body['code']==case['code'] and body['message']==case['message'],record
            # Measure after each response; rejects must leave dispatch count intact.
            state=app.state();assert len(state['invoke'])==len(accepted),state
            assert state['jwksFetches']==0,state
        calls=app.state()['invoke'];assert len({c['requestId'] for c in calls})==len(calls)
        for case,call in zip(accepted,calls):
            assert call['path']==case['path'] and call['method']==case['method'],call
            assert call['body']==case.get('body') and call['signed'],call
            assert call['logType']=='None',call
        receipt=ready(app);assert receipt['config_digest']==manifest['configDigest']==receipt['layer_identity'],receipt
        info=app.inspect();assert info['HostConfig']['ReadonlyRootfs']
        assert all(not m['RW'] for m in info['Mounts'] if m['Destination'] in ['/var/task','/opt'])
        logs=app.logs();assert 'EROFS' not in logs and 'Read-only file system' not in logs
        assert all(s not in logs for s in ['TrialPet','WarmPet'])
        result={'allPassed':True,'requests':len(cases),'statuses':[r['response']['statusCode'] for r in records],'invokeSends':len(calls),'jwksRequests':app.state()['jwksFetches'],'snapshotReadyReports':1,'configDigest':receipt['config_digest'],'readOnlyTaskLayer':True,'initTiming':l2.init_timing(logs),'scope':'local RIE only; owned transport observations, not AWS execution'}
        (out/'result.json').write_text(json.dumps(result,indent=2)+'\n')
    return result

def main():
    p=argparse.ArgumentParser();p.add_argument('--package',required=True,type=Path);p.add_argument('--sha256',required=True);p.add_argument('--out',required=True,type=Path);args=p.parse_args()
    out=args.out.resolve();out.mkdir(parents=True,exist_ok=False)
    assert pack.digest(args.package.read_bytes())==args.sha256,'package identity mismatch'
    checked=pack.check_zip(args.package);(out/'package-check.json').write_text(json.dumps(checked,indent=2)+'\n')
    images={name:json.loads(l2.docker('image','inspect',l2.IMAGES[name]).stdout)[0] for name in ['rie','fake']};(out/'images.json').write_text(json.dumps({'pins':l2.IMAGES,'actual':images},indent=2)+'\n')
    task=out/'var-task';task.mkdir()
    with zipfile.ZipFile(args.package) as z:(task/'bootstrap').write_bytes(z.read('bootstrap'))
    (task/'bootstrap').chmod(0o755)
    stamp=pack.epoch()
    pet,pet_manifest=make_layer(task,out,True,stamp)
    semantic,semantic_manifest=make_layer(task,out,False,stamp)
    pet_cases=[{'id':'pet-valid','path':'/v1/pets','method':'POST','body':'{"name":"TrialPet"}','status':200}, {'id':'pet-invalid','path':'/v1/pets','method':'POST','body':'{}','status':400,'code':'ERR11004','message':'VALIDATOR_SCHEMA'}, {'id':'pet-warm','path':'/v1/pets','method':'POST','body':'{"name":"WarmPet"}','status':200}]
    decimal=[{'id':'decimal-valid','path':'/v1/decimal','method':'POST','body':'{"n":0.3}','status':200},{'id':'decimal-empty','path':'/v1/decimal','method':'POST','body':'{}','status':400,'code':'ERR11004','message':'VALIDATOR_SCHEMA'},{'id':'decimal-warm','path':'/v1/decimal','method':'POST','body':'{"n":0.2}','status':200}]
    matrix=json.loads((ROOT/'apps/light-lambda/tests/l1/data/validator-activation-cases.json').read_text())
    results={}
    for name,opt,manifest,cases in [('petstore',pet,pet_manifest,pet_cases),('decimal',semantic,semantic_manifest,decimal),('semantics',semantic,semantic_manifest,matrix)]:
        results[name]=run_profile(task,opt,manifest,cases,out/name)
    assert sum(x['requests'] for x in results.values())==21
    assert sum(x['invokeSends'] for x in results.values())==13
    assert pack.digest(args.package.read_bytes())==args.sha256
    (out/'results.json').write_text(json.dumps({'allPassed':True,'packageSha256':args.sha256,'profiles':results,'separateInitProbes':{'requests':2,'expectedStatus':400,'invokes':0,'jwks':0},'finalRequests':21,'finalInvokeSends':13,'finalJwksRequests':0,'defaultActivationChanged':False,'awsQualified':False},indent=2)+'\n')
    print(json.dumps(results,indent=2))
if __name__=='__main__':main()
