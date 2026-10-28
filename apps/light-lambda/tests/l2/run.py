#!/usr/bin/env python3
"""L2: real AL2023 RIE + packaged bootstrap; owned fake SDK endpoints only."""
import argparse, copy, datetime, importlib.util, json, os, re, shutil, socket, subprocess, sys, threading, time, urllib.error, urllib.request, uuid, zipfile
from pathlib import Path
APP=Path(__file__).resolve().parents[2];ROOT=APP.parents[1]
spec=importlib.util.spec_from_file_location('package_baseline',APP/'scripts/package_baseline.py');pack=importlib.util.module_from_spec(spec);spec.loader.exec_module(pack)
IMAGES=json.loads((Path(__file__).parent/'images.json').read_text())
def docker(*args,check=True):return subprocess.run(['docker',*map(str,args)],check=check,text=True,stdout=subprocess.PIPE,stderr=subprocess.STDOUT)
def free_port():
    with socket.socket() as s:s.bind(('127.0.0.1',0));return s.getsockname()[1]
def http(url,value=None,timeout=20):
    request=urllib.request.Request(url,data=json.dumps(value).encode() if value is not None else None,headers={'Content-Type':'application/json'})
    try:
        with urllib.request.urlopen(request,timeout=timeout) as response:return response.status,json.load(response)
    except urllib.error.HTTPError as error:
        data=error.read().decode();return error.code,json.loads(data) if data else {}
class Rie:
    def __init__(self,task,opt,out,port=0,*,logger_set=False):
        self.task=task;self.opt=opt;self.out=out;self.port=port or free_port();self.fake_port=free_port();self.name='ll-l2-'+uuid.uuid4().hex[:12];self.fake=self.name+'-fake';self.started=False;self.fake_started=False;self.logger_set=logger_set
    def __enter__(self):
        try:
            args=['run','--detach','--name',self.name,'--platform',IMAGES['platform'],'--read-only','--tmpfs','/tmp:rw,nosuid,nodev,size=128m','--memory','1g','--cpus','2','--pids-limit','128','-p',f'127.0.0.1:{self.port}:8080','-p',f'127.0.0.1:{self.fake_port}:19091','-v',f'{self.task}:/var/task:ro']
            if self.opt is not None:args+=['-v',f'{self.opt}:/opt:ro']
            for key,value in {'AWS_REGION':'us-east-1','AWS_ACCESS_KEY_ID':'test','AWS_SECRET_ACCESS_KEY':'test','AWS_EC2_METADATA_DISABLED':'true','AWS_LAMBDA_FUNCTION_NAME':'light-lambda-l2','AWS_LAMBDA_FUNCTION_VERSION':'1','AWS_LAMBDA_FUNCTION_MEMORY_SIZE':'128','LIGHT_LAMBDA_GATEWAY_ARN':'arn:aws:lambda:us-east-1:000000000000:function:light-lambda-l2','LIGHT_LAMBDA_CONFIG_DIR':'/opt','LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS':'1','RUST_LOG':getattr(self,'log_level','info')}.items():args+=['-e',f'{key}={value}']
            if self.logger_set:args+=['-e','LIGHT_LAMBDA_ALLOW_LOGGER_SET=1']
            docker(*args,IMAGES['rie'],'bootstrap');self.started=True
            fake_source=getattr(self,'fake_source',Path(__file__).parent/'fake_dependencies.py')
            docker('run','--detach','--name',self.fake,'--platform',IMAGES['platform'],'--network','container:'+self.name,'--read-only','--tmpfs','/tmp:rw,nosuid,nodev,size=16m','--memory','128m','--cpus','1','--pids-limit','32','-v',f'{fake_source}:/test/fake_dependencies.py:ro',IMAGES['fake'],'python','-B','/test/fake_dependencies.py');self.fake_started=True
            deadline=time.monotonic()+20
            while True:
                try:
                    http(self.fake_url+'/_ready',timeout=1)
                    with socket.create_connection(('127.0.0.1',self.port),timeout=1):pass
                    break
                except (OSError,urllib.error.URLError):
                    if time.monotonic()>deadline:raise TimeoutError('owned RIE/fake startup timed out')
                    time.sleep(.1)
            return self
        except BaseException:self.__exit__(*sys.exc_info());raise
    @property
    def url(self):return f'http://127.0.0.1:{self.port}/2015-03-31/functions/function/invocations'
    @property
    def fake_url(self):return f'http://127.0.0.1:{self.fake_port}'
    def state(self):return http(self.fake_url+'/_state')[1]
    def logs(self):return docker('logs',self.name,check=False).stdout
    def inspect(self):return json.loads(docker('inspect',self.name).stdout)[0]
    def __exit__(self,*args):
        self.out.mkdir(parents=True,exist_ok=True)
        if self.started:
            (self.out/'rie.log').write_text(self.logs());(self.out/'inspect.json').write_text(json.dumps(self.inspect(),indent=2)+'\n')
        if self.fake_started:
            try:(self.out/'dependencies.json').write_text(json.dumps(self.state(),indent=2)+'\n')
            except Exception:pass
            docker('logs',self.fake,check=False);docker('rm','--force',self.fake,check=False)
        if self.started:docker('rm','--force',self.name,check=False)

def source(directory):
    directory.mkdir()
    for name in ['handler.yml','values.yml']:shutil.copyfile(APP/'examples/local-baseline'/name,directory/name)
    (directory/'lambda-proxy.yml').write_text(json.dumps({'enabled':True,'region':'us-east-1','endpointOverride':'http://127.0.0.1:19091','functions':{'/hello@get':'test-only-business','/backend-error@get':'test-only-business'},'metricsInjection':False}))
    with (directory/'values.yml').open('a') as values:values.write('\nclient.tokenKeyServerUrl: http://127.0.0.1:19091\nclient.tokenKeyUri: /jwks\nclient.tokenKeyEnableHttp2: false\n')
    return directory

def init_timing(logs):
    reported=re.findall(r'Init Duration:\s*([0-9.]+)\s*ms',logs,re.IGNORECASE)
    if reported:return float(reported[0]),'RIE Init Duration'
    stamp=r'(\d{2} [A-Za-z]{3} \d{4} \d{2}:\d{2}:\d{2},\d{3})'
    start=re.search(stamp+r'.*INIT START\(',logs);end=re.search(stamp+r'.*INIT RTDONE\(status: success\)',logs)
    if start and end:
        parse=lambda m:datetime.datetime.strptime(m.group(1),'%d %b %Y %H:%M:%S,%f')
        elapsed=(parse(end)-parse(start)).total_seconds()*1000
        if elapsed>=0:return elapsed,'RIE INIT START to INIT RTDONE log timestamps'
    return None,'not observed'

def invoke(instance,event):
    start=time.monotonic();status,response=http(instance.url,event);return {'httpStatus':status,'response':response,'durationMs':round((time.monotonic()-start)*1000,3)}
def main():
    p=argparse.ArgumentParser();p.add_argument('--package',required=True,type=Path);p.add_argument('--out',required=True,type=Path);p.add_argument('--serve',action='store_true');p.add_argument('--port',type=int,default=0);args=p.parse_args();out=args.out.resolve();out.mkdir(parents=True,exist_ok=False)
    os.environ['LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS']='1'
    checked=pack.check_zip(args.package);(out/'package-check.json').write_text(json.dumps(checked,indent=2)+'\n')
    images={name:json.loads(docker('image','inspect',IMAGES[name]).stdout)[0] for name in ['rie','fake']};(out/'images.json').write_text(json.dumps({'pins':IMAGES,'actual':images},indent=2)+'\n')
    task=out/'var-task';task.mkdir()
    with zipfile.ZipFile(args.package) as archive:(task/'bootstrap').write_bytes(archive.read('bootstrap'))
    (task/'bootstrap').chmod(0o755);baseline=source(out/'source');layer=out/'config-layer.zip';layer_info=pack.layer(baseline,layer,task/'bootstrap');(out/'layer-report.json').write_text(json.dumps(layer_info,indent=2)+'\n');opt=out/'opt';manifest=pack.extract_layer(layer,opt)
    # Rebuild independently after changing only metadata: mtime is not an input.
    os.utime(baseline/'values.yml',(1,1));pack.layer(baseline,out/'config-layer-repeat.zip',task/'bootstrap');assert pack.file_id(layer)['sha256']==pack.file_id(out/'config-layer-repeat.zip')['sha256']
    event=json.loads((APP/'examples/local-baseline/event.json').read_text());results=[]
    with Rie(task,opt,out/'baseline',args.port) as instance:
        if args.serve:
            print(json.dumps({'invocationUrl':instance.url,'fakeStateUrl':instance.fake_url+'/_state','layer':str(layer),'scope':'local RIE only'}),flush=True)
            try:
                while True:time.sleep(1)
            except KeyboardInterrupt:return
        records=[]
        for _ in range(6):
            record=invoke(instance,event);records.append(record);response=record['response'];assert response['statusCode']==200,response;assert response['body']=='hello from business';assert response['multiValueHeaders']['x-business']==['yes']
        bad=copy.deepcopy(event);bad['path']='/backend-error';failure=invoke(instance,bad);records.append(failure);assert failure['response']['statusCode']==502,failure;assert 'ERR14014' in failure['response']['body']
        invalid=copy.deepcopy(event);del invalid['multiValueHeaders'];ingress=invoke(instance,invalid);records.append(ingress);assert ingress['response']['statusCode']==400,ingress;assert 'ERR14012' in ingress['response']['body']
        state=instance.state();calls=state['invoke'];assert len(calls)==7,state;assert [c['eventPath'] for c in calls]==['/hello']*6+['/backend-error'];assert len({c['requestId'] for c in calls})==7;assert all(c['stage']=='local-baseline' and c['signed'] and c['invocationType']=='RequestResponse' and c['logType']=='None' and c['sdkPath'].endswith('/functions/test-only-business/invocations') for c in calls);assert state['jwksFetches']==0
        logs=instance.logs();assert logs.count('immutable snapshot ready')==1,logs;assert 'EROFS' not in logs and 'Read-only file system' not in logs
        ready=[]
        for line in logs.splitlines():
            try:
                row=json.loads(line)
                if row.get('fields',{}).get('message')=='immutable snapshot ready':ready.append(row['fields'])
            except ValueError:pass
        assert len(ready)==1 and ready[0]['config_digest']==manifest['configDigest'] and ready[0]['layer_identity']==manifest['configDigest'],ready
        mounts=instance.inspect()['Mounts'];assert all(not m['RW'] for m in mounts if m['Destination'] in ['/var/task','/opt']);assert instance.inspect()['HostConfig']['ReadonlyRootfs']
        init,init_source=init_timing(logs)
        results.append({'case':'L2-baseline-warm-errors-readonly','pass':True,'enabledHandlers':['proxy'],'coldRequests':1,'warmRequests':5,'sdkInvokes':7,'jwksFetches':0,'initReports':1,'observations':records,'initDurationMs':init,'initDurationSource':init_source,'initBelowOneSecond':init<1000 if init is not None else None})
    for entry in manifest['files']:assert pack.digest((opt/entry['path']).read_bytes())==entry['sha256']
    # Negative layers are deliberately invalid test inputs, not WP14 artifacts.
    for name,error in [('missing-layer','light-lambda.stage'),('validator','enabled handler not supported'),('router-rewrite','Java rewrite execution is unavailable'),('tampered-layer','file digest mismatch')]:
        selected=None
        if name!='missing-layer':
            selected=out/('negative-'+name);shutil.copytree(opt,selected)
            if name=='tampered-layer':(selected/'values.yml').write_text((selected/'values.yml').read_text()+'\n# tampered\n')
            else:
                (selected/'light-lambda-layer.json').unlink()
                alias='validator' if name=='validator' else 'router';cls='com.networknt.aws.lambda.handler.middleware.'+('validator.ValidatorMiddleware' if alias=='validator' else 'router.LambdaRouterMiddleware')
                (selected/'handler.yml').write_text(json.dumps({'handlers':[cls+'@'+alias,'com.networknt.aws.lambda.handler.middleware.proxy.LambdaProxyMiddleware@proxy'],'defaultHandlers':[alias,'proxy']}))
                (selected/('openapi-validator.yml' if alias=='validator' else 'router.yml')).write_text(json.dumps({'enabled':True} if alias=='validator' else {'enabled':True,'urlRewriteRules':['/x /rewritten']}))
                try:pack.validate(task/'bootstrap',selected)
                except ValueError as rejected:assert error in str(rejected),str(rejected)
                else:raise AssertionError('offline Rust Init incorrectly admitted unavailable handler')
        with Rie(task,selected,out/name) as instance:
            try:record=invoke(instance,event)
            except (OSError,urllib.error.URLError,ValueError) as rejected:record={'transportError':type(rejected).__name__}
            logs=instance.logs();text=json.dumps(record)+logs
            if name=='missing-layer':assert 'light-lambda.stage' in text or 'deployment directory' in text,text
            else:assert error in text,text
            assert instance.state()=={'invoke':[],'jwksFetches':0}
            results.append({'case':name,'pass':True,'admissionOnly':True,'sdkInvokes':0,'jwksFetches':0,'observation':record})
    result={'level':'L2 local emulation only','package':checked,'configLayer':pack.file_id(layer),'cases':results,'allPassed':True,'notQualified':['TLS JWKS L2-04 (security disabled baseline)','Init-under-1s unless measured by RIE','all other supported handlers under RIE','precision/Validator','Router rewriting','AWS/freeze-thaw/production resources']};(out/'results.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps({'out':str(out),'casesPassed':len(results),'localOnly':True},indent=2))
if __name__=='__main__':main()
