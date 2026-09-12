#!/usr/bin/env python3
"""Read-only shared-workspace smoke through both deployed Agent/Controller paths."""
import argparse,json,uuid
from pathlib import Path
import websocket
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--token-file',type=Path,required=True,help='Owner bearer credential authorized for both Agents')
parser.add_argument('--task',required=True,help='Existing READY task; this script never provisions or edits it')
parser.add_argument('--workspace',default='personal')
parser.add_argument('--codex-url',default='ws://127.0.0.1:8089/chat')
parser.add_argument('--claude-url',default='ws://127.0.0.1:8090/chat')
parser.add_argument('--report',type=Path,required=True)
args=parser.parse_args()
token=args.token_file.read_text().strip()
reports=[]
for name,url in [('codex',args.codex_url),('claude',args.claude_url)]:
 ws=websocket.create_connection(f'{url}?sessionId={uuid.uuid4()}',header={'Authorization':'Bearer '+token},timeout=240,http_no_proxy=['127.0.0.1','localhost'])
 try:
  while True:
   msg=json.loads(ws.recv())
   if msg['type']=='error':raise RuntimeError(msg)
   if msg['type']=='workspaceCatalog':break
  choice=next(c for c in msg['workspaces'] if c['workspaceId']==args.workspace)
  request=str(uuid.uuid4());instruction='Inspect the existing task using task_workspace. List repositories and read one existing README file if present. Report a brief factual summary. Do not change any files.'
  workspace=dict(schemaVersion=1,requestId=request,workspaceId=args.workspace,expectedMembershipRevision=choice['membershipRevision'],task=dict(kind='existing',taskId=args.task),intent='inspect',instruction=instruction,thread=dict(runnerId=choice['runnerId'],sessionRef=str(uuid.uuid4()),stageId='inspect',mode='new',closeAfterTurn=True))
  ws.send(json.dumps(dict(text=instruction,clientMessageId=request,profile='coding',workspace=workspace)))
  print(name,'submitted read-only existing task',flush=True)
  while True:
   msg=json.loads(ws.recv())
   if msg['type']=='error':raise RuntimeError(msg)
   if msg['type']=='executionResult':
    reports.append(dict(adapter=name,**msg));args.report.write_text(json.dumps(reports,indent=2))
    assert msg['state']=='COMPLETED',msg
    assert msg['codingThread']['state']=='CLOSED',msg
    assert msg['workspace']['taskId']==args.task,msg
    print(name,'COMPLETED, conversation CLOSED',flush=True);break
 finally:ws.close()

assert len({r["workspace"]["checkpointDigest"] for r in reports}) == 1, "Read-only adapters observed different task checkpoints"
