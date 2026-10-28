#!/usr/bin/env python3
"""Run the fixed WP13 closure/forwarding selection through the existing native runner. No worker experiments."""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time

repo=Path(__file__).resolve().parents[3]
java_repo=repo.parent/'light-lambda-native-conformance'
if len(sys.argv)!=2:
    raise SystemExit('usage: python3 apps/light-lambda/scripts/run-java-closure-fixtures.py <fresh-output-dir>')
out=Path(sys.argv[1]).resolve()
out.mkdir()
results=out/'results';results.mkdir()
history=[]

def execute(name,argv,timeout=180):
    start=time.monotonic()
    with (out/(name+'.log')).open('x') as log:
        process=subprocess.Popen(['rtk','proxy',*map(str,argv)],cwd=java_repo,stdout=log,stderr=subprocess.STDOUT,start_new_session=True)
        try: code=process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid,signal.SIGTERM)
            try:process.wait(timeout=3)
            except subprocess.TimeoutExpired:os.killpg(process.pid,signal.SIGKILL);process.wait()
            code=124
    item=dict(name=name,argv=list(map(str,argv)),exit=code,elapsedSeconds=time.monotonic()-start)
    with (out/(name+'.json')).open('x') as f:json.dump(item,f,indent=2)
    history.append(item)
    return code

if execute('test-compile',['mvn','-o','-q','test-compile','dependency:build-classpath','-Dmdep.outputFile=target/conformance-cp.txt'])!=0:
    raise SystemExit('Java test compilation failed; see '+str(out))
classpath=os.pathsep.join([str(java_repo/'target/test-classes'),str(java_repo/'target/classes'),(java_repo/'target/conformance-cp.txt').read_text().strip()])
revision=subprocess.check_output(['rtk','git','rev-parse','HEAD'],cwd=java_repo,text=True).strip()
failed=0
selected=json.loads((repo/'tests/serverless-conformance/metadata/wp13-closure-selection-v1.json').read_text())['fixtures']
by_id={json.loads(p.read_text())['id']:p for p in (repo/'tests/serverless-conformance/fixtures').rglob('*.json')}
fixtures=[by_id[id] for id in selected]
for fixture in fixtures:
    with tempfile.TemporaryDirectory(prefix='native-closure-conformance-') as config:
        argv=['env','-u','LIGHT_LAMBDA_ALLOW_INSECURE_TEST_CONFIG','AWS_EC2_METADATA_DISABLED=true','java',
              '-Dconformance.home='+str(repo/'tests/serverless-conformance'),
              '-Dlogback.configurationFile='+str(java_repo/'src/test/resources/conformance-logback.xml'),
              '-Dconformance.runnerRevision='+revision,'-Dlight-4j-config-dir='+config,
              '-cp',classpath,'com.networknt.aws.lambda.conformance.ConformanceRunnerMain',str(fixture),str(results/fixture.name)]
        failed+=execute(fixture.stem,argv,timeout=45)!=0
with (out/'commands.json').open('x') as f:json.dump(history,f,indent=2)
print(f'Native closure fixtures: {len(fixtures)-failed} passed, {failed} failed; results={results}')
raise SystemExit(bool(failed))
