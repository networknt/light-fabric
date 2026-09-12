#!/usr/bin/env python3
"""Disposable cross-adapter workspace/session qualification. Consumes native subscriptions."""
import argparse, copy, hashlib, json, os
from pathlib import Path
import queue, subprocess, tempfile, threading, time, uuid


def invoke(worker, profile, spec, config, native, home):
    claude = profile['adapterId'] == 'claude-code-v1'
    keys = 'schemaVersion adapterId adapterVersion adapterProtocolVersion actionKind compatibilityDigest imageDigest capabilityDigest templateId templateVersion templateDigest executable binaryDigest schemaDigest requiredFeatures'.split()
    caps=json.loads(subprocess.check_output([worker,'print-capabilities']))
    identity=dict(executionId=str(uuid.uuid4()),leaseId=str(uuid.uuid4()),fencingToken=1,transportNonce=uuid.uuid4().hex)
    hello=dict(type='hello',identity=identity,expected_capability_digest=caps['capabilityDigest'])
    payload=dict(workspaceSpec=spec,adapterContract={k:profile[k] for k in keys},adapterQualification=profile['qualification'])
    if claude: payload['claudePolicy']=profile['claudePolicy']
    start=dict(type='start',session_id=str(uuid.uuid4()),turn_id=str(uuid.uuid4()),action_attempt_id=str(uuid.uuid4()),policy_digest='sha256:'+'a'*64,deadline_ms=240000,input=payload)
    env={k:v for k,v in os.environ.items() if not k.startswith(('LIGHT_CODEX_','LIGHT_CLAUDE_'))}
    prefix='LIGHT_CLAUDE_' if claude else 'LIGHT_CODEX_'
    env.update({prefix+'EXECUTABLE':str(native.resolve()),prefix+'HOME':str(home.resolve()),'LIGHT_WORKSPACE_CONFIG':str(config)})
    with tempfile.TemporaryFile(mode='w+') as errors:
        child=subprocess.Popen([worker],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=errors,text=True,env=env)
        try:
            child.stdin.write(json.dumps(hello)+'\n'+json.dumps(start)+'\n');child.stdin.flush()
            lines=queue.Queue()
            def read():
                for line in child.stdout:lines.put(line)
                lines.put(None)
            threading.Thread(target=read,daemon=True).start()
            deadline=time.monotonic()+250
            while time.monotonic()<deadline:
                try:line=lines.get(timeout=1)
                except queue.Empty:continue
                if line is None:raise RuntimeError('worker exited before terminal')
                event=json.loads(line)['payload']
                if event['type']=='progress': print(event['message'],flush=True)
                if event['type']=='terminal':
                    if event['class']!='success':raise RuntimeError(event.get('error','worker failure'))
                    return event['output']
            raise TimeoutError('shared workspace turn timed out')
        finally:
            child.terminate()
            try:child.wait(timeout=10)
            except subprocess.TimeoutExpired:child.kill();child.wait()


def main():
    p=argparse.ArgumentParser(description=__doc__)
    for arg in ['codex-profile','claude-profile','codex','claude','codex-home','claude-home','report']:p.add_argument('--'+arg,type=Path,required=True)
    p.add_argument('--fabric',type=Path,default=Path(__file__).resolve().parents[1]);args=p.parse_args()
    profiles={name:json.loads(getattr(args,name+'_profile').read_text()) for name in ['codex','claude']}
    with tempfile.TemporaryDirectory(prefix='shared-coding-') as temporary:
        root=Path(temporary);repo=root/'repo';repo.mkdir()
        subprocess.run(['git','init','-q','-b','develop',repo],check=True)
        (repo/'README.md').write_text('SHARED_BASE\n')
        subprocess.run(['git','-C',repo,'add','.'],check=True)
        subprocess.run(['git','-C',repo,'-c','user.name=Smoke','-c','user.email=smoke@example.invalid','commit','-qm','base'],check=True)
        repos=[dict(name='example',source=str(repo),integrationBranch='develop',releaseBranch='master')]
        registration=dict(schemaVersion=1,id='shared',hostId='host',agents=['codex','claude'],repositories=repos,operations=['edit','review'],indexers={})
        source=root/'registration.json';source.write_text(json.dumps(registration));store=root/'store'
        subprocess.run([args.fabric/'target/debug/light-workspace',store,'register',source],check=True,stdout=subprocess.DEVNULL)
        revision='sha256:'+hashlib.sha256(json.dumps([1,'shared','host',repos],separators=(',',':')).encode()).hexdigest()
        bindings={name:dict(schemaVersion=1,workspaceId='shared',hostId='host',environment='dev',runnerId=name+'-runner',membershipRevision=revision,authorizationRevision=1,subjects=['owner'],agents=['codex','claude'],intents=['inspect','implement','review']) for name in profiles}
        configs={}
        for name in profiles:
            configs[name]=root/(name+'.json');configs[name].write_text(json.dumps(dict(store=str(store),bindings=[bindings[name]])));configs[name].chmod(0o600)
        outputs=[]
        def turn(name, intent, instruction, previous=None, task=None, checkpoint=None, close=False):
            thread=dict(runnerId=name+'-runner',sessionRef=str(uuid.uuid4()),stageId=intent,mode='new',closeAfterTurn=False)
            if previous:
                thread.update(sessionRef=previous['codingThread']['sessionRef'],expectedCheckpoint=previous['codingThread']['checkpoint'],mode='close' if close else 'resume')
            request=dict(schemaVersion=1,requestId=str(uuid.uuid4()),workspaceId='shared',expectedMembershipRevision=revision,
                task=dict(kind='existing',taskId=task) if task else dict(kind='new',description='Cross adapter review'),intent=intent,expectedCheckpointDigest=checkpoint,instruction=instruction,thread=thread)
            print(name,intent,thread['mode'],flush=True)
            result=invoke(args.fabric/'target/debug'/('light-claude-worker' if name=='claude' else 'light-agent-worker'),profiles[name],dict(request=request,binding=bindings[name],subject='owner',agentId=name),configs[name],getattr(args,name),getattr(args,name+'_home'))
            diagnostic=args.report.with_name(args.report.stem+f'-{len(outputs)}.private.json')
            diagnostic.touch(mode=0o600,exist_ok=True);diagnostic.chmod(0o600)
            diagnostic.write_text(json.dumps(result,indent=2))
            outputs.append(dict(adapter=name,intent=intent,mode=thread['mode'],workspace=result['workspace'],thread=result['codingThread']))
            return result
        marker='IMPL_'+uuid.uuid4().hex; review_marker='REVIEW_'+uuid.uuid4().hex
        a=turn('codex','implement','Use task_workspace: list repositories; read README.md in repository example; then edit that file with the returned digest, appending FIRST_CHANGE on its own line. Check the edit tool succeeded and reread to verify it. Remember '+marker+' as a non-secret test marker in this conversation; do not write it to files.')
        task_record=json.loads(next(store.rglob('task.json')).read_text());checkout=Path(task_record['checkouts'][0]['path'])
        assert (checkout/'README.md').read_text().splitlines()==['SHARED_BASE','FIRST_CHANGE'], 'implementation did not produce the requested edit'
        task=a['workspace']['taskId'];checkpoint=a['workspace']['checkpointDigest']
        b=turn('claude','review','Read README.md. Report FIRST_CHANGE. Remember '+review_marker+' as a non-secret test marker in this conversation. Do not change files.',task=task,checkpoint=checkpoint)
        assert 'FIRST_CHANGE' in b['finalMessage'] and b['workspace']['checkpointDigest']==checkpoint
        a=turn('codex','implement','Recall the non-secret test IMPL marker in your final answer. Read README.md in repository example, edit with the current digest to append SECOND_CHANGE on its own line. Verify the edit succeeded by rereading.',previous=a,task=task,checkpoint=checkpoint)
        assert marker in a['finalMessage'];checkpoint=a['workspace']['checkpointDigest']
        b=turn('claude','review','Read the CURRENT README.md again. Report SECOND_CHANGE and recall the non-secret test REVIEW marker. Do not modify files.',previous=b,task=task,checkpoint=checkpoint)
        assert review_marker in b['finalMessage'] and 'SECOND_CHANGE' in b['finalMessage'] and b['workspace']['checkpointDigest']==checkpoint
        task_record=json.loads(next(store.rglob('task.json')).read_text());checkout=Path(task_record['checkouts'][0]['path'])
        assert (checkout/'README.md').read_text()=='SHARED_BASE\nFIRST_CHANGE\nSECOND_CHANGE\n'
        a=turn('codex','implement','This is purely conversational. Do not use any tools. Reply with the non-secret test IMPL marker you remember; no repository facts or edits are requested.',previous=a,task=task,checkpoint=checkpoint)
        b=turn('claude','review','This is purely conversational. Do not use any tools. Reply with the non-secret test REVIEW marker you remember; no repository facts or edits are requested.',previous=b,task=task,checkpoint=checkpoint)
        assert marker in a['finalMessage'] and review_marker in b['finalMessage']
        assert a['workspace']['checkpointDigest']==checkpoint and b['workspace']['checkpointDigest']==checkpoint
        for name,intent,previous in [('codex','implement',a),('claude','review',b)]:
            closed=turn(name,intent,'Close this conversation.',previous=previous,task=task,checkpoint=checkpoint,close=True)
            assert closed['codingThread']['state']=='CLOSED'
        args.report.write_text(json.dumps(dict(status='passed',boundary='native-workers-shared-store',crossAdapterReview=True,independentFileValidation=True,turns=outputs),indent=2)+'\n')
        print('Passed:',args.report)


if __name__=='__main__':main()
