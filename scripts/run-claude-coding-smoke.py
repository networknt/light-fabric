#!/usr/bin/env python3
"""Opt-in native Phase 2 worker proof. Never copies credentials or enables routing."""
import argparse, copy, hashlib, json, os, pathlib, subprocess, tempfile, uuid

def digest(data):
    return 'sha256:' + hashlib.sha256(data).hexdigest()

def canonical(value):
    return digest(json.dumps(value,sort_keys=True,separators=(',',':'),ensure_ascii=False).encode())

def main():
    parser=argparse.ArgumentParser()
    parser.add_argument('--claude',required=True)
    parser.add_argument('--native-home',required=True)
    parser.add_argument('--report',required=True)
    parser.add_argument('--permission-source',choices=['agent-policy','claude-cli'],default='agent-policy')
    parser.add_argument('--worker',default='target/debug/examples/claude-coding')
    parser.add_argument('--runner',help='Use the real runner dispatch driver with --worker light-claude-worker')
    args=parser.parse_args()
    worker=str(pathlib.Path(args.worker).resolve())
    results=[]
    if args.runner:
        caps=json.loads(subprocess.check_output([worker,'print-capabilities']))
        contract=dict(schemaVersion=1,adapterId='claude-code-v1',adapterVersion='2.1.269',adapterProtocolVersion='claude-cli-stream-json-v1',
            actionKind='coding.claude-code-v1',compatibilityDigest=digest(b'claude-phase2-runner'),imageDigest=digest(pathlib.Path(worker).read_bytes()),
            capabilityDigest=caps['capabilityDigest'],templateId='coding-claude-code-v1',templateVersion=1,templateDigest=digest(b'claude-phase2-template'),
            executable='/usr/local/bin/claude',binaryDigest=digest(pathlib.Path(args.claude).read_bytes()),
            schemaDigest=digest(pathlib.Path('contracts/claude-code/v2.1.269/phase2-launch.json').read_bytes()),
            requiredFeatures=sorted(['claude-code-v1','canonical-patch-output','workflow-coding-threads-v1','claude-review-namespace-v1']))
        evidence=pathlib.Path('contracts/claude-code/v2.1.269/phase2-qualification.json')
        qualification=dict(schemaVersion=1,adapterId=contract['adapterId'],adapterVersion=contract['adapterVersion'],status='local-qualified',
            evaluatedDimensions=sorted(json.loads(evidence.read_text())['dimensions']),contractDigest=canonical(contract),evidenceDigest=digest(evidence.read_bytes()))
        config=dict(originServiceId='com.networknt.agent.account-1.0.0',executable=worker,binaryDigest=contract['imageDigest'],
            capabilityDigest=caps['capabilityDigest'],claudeHome=str(pathlib.Path(args.native_home).resolve()),claudeExecutable=str(pathlib.Path(args.claude).resolve()))
    with tempfile.TemporaryDirectory(prefix='claude-coding-proof-') as temporary:
        root=pathlib.Path(temporary); source=root/'source'; source.mkdir()
        def git(*arguments):
            return subprocess.check_output(['/usr/bin/git',*arguments],cwd=source,stderr=subprocess.DEVNULL).decode().strip()
        git('init','-q');git('config','user.name','Qualification');git('config','user.email','qualification@example.invalid')
        (source/'main.py').write_text('def add(a, b):\n    return a - b\n')
        git('add','.');git('commit','-qm','base')
        base=git('rev-parse','HEAD');bundle=root/'repository.bundle';git('bundle','create',str(bundle),'HEAD')
        manifest=dict(schemaVersion=1,materializerId='coding',materializerVersion=1,productProfile='coding',
            runtimeCompatibility='claude-code-v1',packages=[],effectiveInstructions=[],allowedTools=[],writableRoots=['/workspace/repository'])
        if args.runner: manifest['runtimeCompatibility']=contract['compatibilityDigest']
        scope=digest(uuid.uuid4().bytes)
        turn=dict(coding=dict(thread=dict(runnerId='phase2-native',sessionRef=str(uuid.uuid4()),stageId='phase2',mode='new',closeAfterTurn=False),
            repositoryDigest=digest(bundle.read_bytes()),baseRevision=base,workspaceRoot='/workspace/repository',
            prompt='',modelAlias='coding-implementer',authenticationProfile='personal-subscription',role='implement',
            roleProfile=dict(profileId='coding-implement-v1',modelAlias='coding-implementer',workspaceAuthority='bounded-write'),
            materializationManifestDigest=canonical(manifest),writableRoots=manifest['writableRoots'],
            allowedTools=['fs.read','fs.write','process.exec'],maximumPatchBytes=4096,maximumChangedFiles=10),
            policy=dict(permissionSource='agent-policy',permissionMode='bypassPermissions',defaultModel='sonnet',
                models={'sonnet':'claude-sonnet-5'},tools=['Read','Edit','Write','Bash'],allowedTools=[]),nativeModel='sonnet')
        if args.permission_source=='claude-cli':
            turn['policy'].update(permissionSource='claude-cli',tools=[],allowedTools=[])
        def run(label, request):
            print('Native qualification: '+label,flush=True)
            local=copy.deepcopy(manifest);local['writableRoots']=request['coding']['writableRoots']
            request['coding']['materializationManifestDigest']=canonical(local)
            path=root/'request.json'
            if args.runner:
                staged=dict(inputId=str(uuid.uuid4()),sourceDigest=request['coding']['repositoryDigest'],localPath=str(bundle),mountTarget='/inputs/repository.bundle',
                    mediaType='application/x-git-bundle',size=bundle.stat().st_size,readOnly=True,executable=False,mountOptions=['ro'])
                spec=dict(schemaVersion=1,templateDigest=contract['templateDigest'],expectedCapabilityDigest=caps['capabilityDigest'],
                    sessionId=str(uuid.uuid4()),turnId=str(uuid.uuid4()),actionAttemptId=str(uuid.uuid4()),policyDigest=digest(b'phase2-agent-policy'),
                    input=dict(codingSpec=request['coding'],claudePolicy=request['policy'],nativeModel=request['nativeModel'],
                        threadScope=scope,materializationManifest=local,adapterContract=contract,adapterQualification=qualification,runtimeStagedInputs=[staged]),
                    wallClockTimeoutMs=180000,maximumEventBytes=1048576,maximumStderrBytes=1048576)
                path.write_text(json.dumps(dict(config=config,spec=spec)))
                command=[str(pathlib.Path(args.runner).resolve()),str(path),str(root/'journal.sqlite')]
            else:
                path.write_text(json.dumps(dict(turn=request,manifest=local)))
                command=[worker,str(pathlib.Path(args.claude).resolve()),str(pathlib.Path(args.native_home).resolve()),str(bundle),scope,str(path)]
            process=subprocess.run(command,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,timeout=200)
            if process.returncode:
                raise RuntimeError(label+' failed: '+process.stderr[-2000:])
            out=json.loads(process.stdout)
            if args.runner:
                assert out['events'] >= 2 and out['fencingToken']==7
                value=out['output']
                out=value.get('worker',value)
                out['patch']=value.get('codingPatch')
            results.append(dict(turn=label,state=out['codingThread']['state'],nativeModel=out['nativeModel'],
                credentialSource=(out['authentication'] or {}).get('credentialSource'),
                patchDigest=(out['patch'] or {}).get('patchDigest'),verdict=(out['codingReview'] or {}).get('verdict')))
            return out
        def resume(request,out):
            new=copy.deepcopy(request);new['nativeModel']=None
            new['coding']['thread'].update(mode='resume',expectedCheckpoint=out['codingThread']['checkpoint'])
            return new
        marker='memory-'+uuid.uuid4().hex
        turn['coding']['prompt']=f'Fix add in main.py to add integers. Run python3 -B -c "from main import add; assert add(2, 3) == 5". Remember this conversation-only marker: {marker}. Do not write the marker to a file. Do not create any other files. Return the marker in your final answer.'
        first=run('implement-new',turn)
        assert marker in first['finalMessage']
        assert '+    return a + b' in first['patch']['patch']
        review=copy.deepcopy(turn)
        review['coding'].update(role='review',modelAlias='coding-reviewer',roleProfile=dict(profileId='coding-review-v1',modelAlias='coding-reviewer',workspaceAuthority='review-read-only'),
            writableRoots=['/workspace/review-scratch'],allowedTools=['fs.read','process.exec'])
        review['coding']['thread']['sessionRef']=str(uuid.uuid4())
        def review_input(out):
            patch=out['patch']
            return dict(reviewId='phase2-review',repository='qualification/example',requirements='add must correctly add numbers',
                requirementsDigest=digest(b'add must correctly add numbers'),candidatePatch=patch['patch'],
                implementation=dict(schemaVersion=1,adapterContractDigest=digest(b'phase2-local-contract'),repositoryDigest=turn['coding']['repositoryDigest'],
                    baseRevision=base,patchDigest=patch['patchDigest'],changedPaths=patch['changedPaths'],validationEvidence=[],resolvedFindingIds=[]))
        review['coding']['reviewInput']=review_input(first)
        review_marker='review-'+uuid.uuid4().hex
        review['coding']['prompt']=f'Review main.py. Run a Python assertion using python3 -B. Remember {review_marker} only in this conversation and include it in validationGaps as a qualification marker. Return approved if correct. Do not modify files.'
        reviewed=run('review-new',review)
        assert reviewed['codingReview']['verdict']=='approved'
        follow=resume(turn,first)
        follow['coding']['prompt']='Recall the conversation-only memory marker from the previous turn and return it in your final answer. Add exactly the module docstring """Integer addition.""" to main.py, keep add correct, and run the same Python assertion. Do not create other files.'
        second=run('implement-resume',follow)
        assert marker in second['finalMessage']
        assert 'Integer addition.' in second['patch']['patch']
        refresh=resume(review,reviewed);refresh['coding']['reviewInput']=review_input(second)
        refresh['coding']['thread']['closeAfterTurn']=True
        refresh['coding']['prompt']='Review the newly refreshed candidate; confirm it now has the Integer addition. module docstring and still correctly adds. Run a Python assertion using python3 -B. Recall the conversation-only review marker from the prior turn and include it in validationGaps. Return approved only if correct.'
        final=run('review-resume-close',refresh)
        assert final['codingThread']['state']=='CLOSED' and final['codingReview']['verdict']=='approved'
        assert review_marker in str(final['codingReview']['validationGaps'])
        close=resume(follow,second);close['coding']['thread']['mode']='close'
        closed=run('implement-close',close);assert closed['codingThread']['state']=='CLOSED'
        # Reconstruct the actual accepted patch and run a fixed independent test.
        subprocess.run(['/usr/bin/git','apply','-'],input=second['patch']['patch'],text=True,cwd=source,check=True)
        subprocess.run(['/usr/bin/python3','-B','-c','from main import add; assert add(2,3)==5; assert add(-2,3)==1'],cwd=source,check=True)
    report=dict(schemaVersion=1,adapter='claude-code-v1',workerBoundary=('runner-admission-fenced-runtime-and-journal' if args.runner else 'separate-local-process-per-turn'),
        productionQualified=False,permissionSource=args.permission_source,checks=dict(implementationContext=True,independentReviewContext=True,refreshedCandidate=True,
        close=True,nativeAuth=True,canonicalPatch=True,independentTest=True),turns=results)
    pathlib.Path(args.report).write_text(json.dumps(report,indent=2)+'\n')
    print('Claude Phase 2 native coding proof passed: '+args.report)
if __name__=='__main__':
    main()
