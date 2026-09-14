#!/usr/bin/env python3
"""Prepare the local A2 PKI, app tokens and resolved service configuration."""
import argparse, base64, hashlib, json, os, subprocess, time
from pathlib import Path
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import padding

ISSUER='urn:com:networknt:oauth2:v1'; AUDIENCE='urn:com.networknt'

def run(*args, cwd=None): subprocess.run(args, cwd=cwd, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
def b64(v): return base64.urlsafe_b64encode(v).decode().rstrip('=')
def canonical(v): return json.dumps(v,sort_keys=True,separators=(',',':')).encode()
def private(path,data):
    path.parent.mkdir(parents=True,exist_ok=True,mode=0o700)
    fd=os.open(path,os.O_WRONLY|os.O_CREAT|os.O_TRUNC,0o600)
    with os.fdopen(fd,'wb') as f: f.write(data if isinstance(data,bytes) else data.encode())
def cert(ca, name, san, usage, out):
    run('openssl','req','-new','-newkey','rsa:2048','-nodes','-keyout',name+'.key','-out',name+'.csr','-subj','/CN='+name,cwd=out)
    (out/(name+'.ext')).write_text('basicConstraints=CA:FALSE\nextendedKeyUsage='+usage+'\nsubjectAltName='+san+'\n')
    run('openssl','x509','-req','-in',name+'.csr','-CA',str(ca/'ca.pem'),'-CAkey',str(ca/'ca.key'),'-CAcreateserial','-out',name+'.pem','-days','3650','-sha256','-extfile',name+'.ext',cwd=out)
def fp(path):
    der=subprocess.check_output(['openssl','x509','-in',str(path),'-outform','DER'])
    return hashlib.sha256(der).hexdigest()

def main():
    ap=argparse.ArgumentParser(); ap.add_argument('--output',type=Path,required=True); ap.add_argument('--identities',type=Path,default=Path(__file__).with_name('identities.json')); a=ap.parse_args()
    ids=json.loads(a.identities.read_text()); out=a.output.resolve()
    out.mkdir(parents=True,exist_ok=False,mode=0o700); pki=out/'pki'; pki.mkdir(mode=0o700)
    run('openssl','req','-x509','-newkey','rsa:3072','-nodes','-keyout','ca.key','-out','ca.pem','-days','3650','-sha256','-subj','/CN=Light API local A2 CA',cwd=pki)
    cert(pki,'workflow-server','DNS:light-workflow','serverAuth',pki)
    cert(pki,'gateway-client','URI:spiffe://lightapi.local/gateway/workflow-actions','clientAuth',pki)
    cert(pki,'workflow-client','URI:spiffe://lightapi.local/workflow/action-producer','clientAuth',pki)
    cert(pki,'codex-client','URI:spiffe://lightapi.local/agent/codex-workflow','clientAuth',pki)
    cert(pki,'claude-client','URI:spiffe://lightapi.local/agent/claude-workflow','clientAuth',pki)
    cert(pki,'codex-server','DNS:light-agent-codex-personal-workflow','serverAuth',pki)
    cert(pki,'claude-server','DNS:light-agent-claude-personal-workflow','serverAuth',pki)
    # Use the current local issuer key only for this explicitly local development profile.
    inspect=json.loads(subprocess.check_output(['docker','inspect','light-gateway']))[0]
    env=dict(x.split('=',1) for x in inspect['Config']['Env'] if '=' in x)
    original=env['LIGHT_PORTAL_AUTHORIZATION'].removeprefix('Bearer ')
    header=json.loads(base64.urlsafe_b64decode(original.split('.')[0]+'=='))
    kid=header['kid']; assert all(c.isalnum() or c in '-_' for c in kid)
    material=subprocess.check_output(['docker','exec','postgres','psql','-U','postgres','-d','configserver','-Atc',"SELECT private_key FROM auth_provider_key_t WHERE kid='"+kid+"';"],text=True).strip()
    if 'BEGIN' not in material: material='-----BEGIN PRIVATE KEY-----\n'+material+'\n-----END PRIVATE KEY-----\n'
    key=serialization.load_pem_private_key(material.encode(),password=None); del material
    now=int(time.time())
    def token(service):
        claims={'iss':ISSUER,'aud':AUDIENCE,'sub':service,'cid':service,'client_id':service,'sid':service,'host':ids['hostId'],'env':'dev','scp':['portal.r','portal.w'],'scope':'portal.r portal.w','token_use':'app','iat':now,'nbf':now-30,'exp':now+3650*86400}
        msg=(b64(canonical({'alg':'RS256','typ':'JWT','kid':kid}))+'.'+b64(canonical(claims))).encode()
        return 'Bearer '+msg.decode()+'.'+b64(key.sign(msg,padding.PKCS1v15(),hashes.SHA256()))+'\n'
    services={'gateway':ids['gatewayServiceId'],'workflow':ids['workflowServiceId'],'codex':ids['codex']['serviceId'],'claude':ids['claude']['serviceId']}
    for name,service in services.items(): private(out/name/'pki'/'scope-token',token(service))
    # Copy certificates into per-service least-access mount trees.
    copies={
      'workflow':['workflow-server.pem','workflow-server.key','gateway-client.pem','codex-client.pem','claude-client.pem','ca.pem','workflow-client.pem','workflow-client.key'],
      'gateway':['gateway-client.pem','gateway-client.key','workflow-client.pem','workflow-client.key','ca.pem'],
      'codex':['codex-client.pem','codex-client.key','codex-server.pem','codex-server.key','workflow-server.pem','ca.pem'],
      'claude':['claude-client.pem','claude-client.key','claude-server.pem','claude-server.key','workflow-server.pem','ca.pem']}
    for component,names in copies.items():
      target=out/component/'pki'; target.mkdir(parents=True,exist_ok=True,mode=0o700)
      for name in names: private(target/name,(pki/name).read_bytes())
    gateway_fp=fp(pki/'gateway-client.pem'); workflow_fp=fp(pki/'workflow-client.pem'); codex_fp=fp(pki/'codex-client.pem'); claude_fp=fp(pki/'claude-client.pem')
    apps={ids['gatewayServiceId']:{'origin':'gateway','peerSha256':[gateway_fp]},ids['codex']['serviceId']:{'origin':'workflow','peerSha256':[codex_fp]},ids['claude']['serviceId']:{'origin':'workflow','peerSha256':[claude_fp]}}
    policy={'issuer':ISSUER,'audience':AUDIENCE,'hostId':ids['hostId'],'apps':apps,'legacyLongLivedAppKeys':[]}
    action={'workflowAgents':{ids['codex']['serviceId']:ids['codex']['agentDefId'],ids['claude']['serviceId']:ids['claude']['agentDefId']},'receivers':{},'outbound':{'gatewayUrl':'https://light-gateway:8443/mcp','serviceId':ids['workflowServiceId'],'clientIdentityFile':'/run/workflow-actions/pki/workflow-client-identity.pem','caFile':'/config/ca.pem','scopeTokenFile':'/run/workflow-actions/pki/scope-token','maximumDepth':4,'requestByteLimit':1048576,'responseByteLimit':1048576,'costUnitLimit':1000000},'tls':{'address':'0.0.0.0:8449','certificateFile':'/run/workflow-actions/pki/workflow-server.pem','privateKeyFile':'/run/workflow-actions/pki/workflow-server.key','clientCaFile':'/run/workflow-actions/pki/ca.pem'},'policy':policy,'owners':{gateway_fp:{'gatewayService':ids['gatewayServiceId'],'replica':ids['gatewayReplicaId']}}}
    private(out/'workflow'/'action-authorization.json',json.dumps(action,separators=(',',':')))
    gateway={'authorization':{'gatewayUrl':'https://light-gateway:8443/mcp','policy':{'issuer':ISSUER,'audience':AUDIENCE,'hostId':ids['hostId'],'apps':{ids['workflowServiceId']:{'origin':'workflow','peerSha256':[workflow_fp]}},'legacyLongLivedAppKeys':[]},'incomingClientCaFile':'/run/workflow-actions/ca.pem','control':{'baseUrl':'https://light-workflow:8449/','clientIdentityFile':'/run/workflow-actions/gateway-client-identity.pem','caFile':'/run/workflow-actions/ca.pem','scopeTokenFile':'/run/workflow-actions/scope-token','owner':{'gatewayService':ids['gatewayServiceId'],'replica':ids['gatewayReplicaId']}},'backendCaFile':'/run/workflow-actions/ca.pem','backendCertificateFile':'/run/workflow-actions/gateway-client.pem','backendKeyFile':'/run/workflow-actions/gateway-client.key','backendScopeTokenFile':'/run/workflow-actions/scope-token','targets':{}}}
    private(out/'gateway'/'workflow-actions.yml',('authorization: '+json.dumps(gateway['authorization'],separators=(',',':'))+'\n'))
    # reqwest identities require certificate and private key in one PEM.
    for component,name in [('workflow','workflow-client'),('gateway','gateway-client'),('codex','codex-client'),('claude','claude-client')]:
      d=out/component/'pki'; private(d/(name+'-identity.pem'),(d/(name+'.pem')).read_bytes()+(d/(name+'.key')).read_bytes())
    for name in ('codex','claude'):
      x=ids[name]; cfg={'authorization':{'mode':'workflow','serviceId':x['serviceId'],'agentDefId':x['agentDefId'],'incoming':{'issuer':ISSUER,'audience':AUDIENCE,'hostId':ids['hostId'],'apps':{ids['gatewayServiceId']:{'origin':'gateway','peerSha256':[gateway_fp]}},'legacyLongLivedAppKeys':[]},'tls':{'address':'0.0.0.0:8450','certificateFile':'/run/workflow-actions/'+name+'-server.pem','privateKeyFile':'/run/workflow-actions/'+name+'-server.key','clientCaFile':'/run/workflow-actions/ca.pem'},'jobAuthorization':{'baseUrl':'https://light-workflow:8449/','clientIdentityFile':'/run/workflow-actions/'+name+'-client-identity.pem','caFile':'/run/workflow-actions/ca.pem','scopeTokenFile':'/run/workflow-actions/scope-token'}}}
      private(out/name/'workflow-origin.yml','authorization: '+json.dumps(cfg['authorization'],separators=(',',':'))+'\n')
    manifest={'schemaVersion':1,'status':'prepared','hostId':ids['hostId'],'identities':ids,'certificateSha256':{'gateway':gateway_fp,'workflow':workflow_fp,'codex':codex_fp,'claude':claude_fp},'operationalMigration':'0007_workflow_action_dispatch','secretsIncluded':False}
    private(out/'manifest.json',json.dumps(manifest,indent=2)+'\n')
    for path in pki.iterdir(): path.unlink()
    pki.rmdir()

if __name__=='__main__': main()
