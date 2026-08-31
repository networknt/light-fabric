import base64, hashlib, hmac, json, os, socket, tempfile, unittest, urllib.error, urllib.request, uuid
from dataclasses import replace
from datetime import datetime, timedelta, timezone
from pathlib import Path
from light_a2a_backend import AdapterConfig, ReplayStore, serve, verify

REQUIRED_CASES = {
    "valid-unary", "valid-stream", "status", "cancel", "artifact", "deadline",
    "business-error", "restart-reconciliation", "forged-signature", "expired",
    "replayed", "wrong-audience", "wrong-host", "wrong-environment",
    "wrong-agent", "wrong-skill", "wrong-operation", "wrong-task",
    "wrong-context", "wrong-idempotency", "wrong-publication", "wrong-policy",
    "wrong-data-boundary", "unconfigured-destination",
}

class Headers(dict):
    def get(self, key, default=None): return super().get(key.lower(), default)

def signed_headers(body, context, key, digest):
    encoded = base64.urlsafe_b64encode(json.dumps(context,separators=(",",":")).encode()).decode().rstrip("=")
    signature = hmac.new(key, encoded.encode()+b"\0"+body, hashlib.sha256).hexdigest()
    return Headers({"x-light-a2a-backend-context":encoded,"x-light-a2a-backend-signature":signature,"x-light-a2a-backend-contract-digest":digest})

def vector(operation="INVOKE"):
    key=b"k"*32; task=str(uuid.uuid4()); context_id=str(uuid.uuid4()); replay_path=Path(tempfile.mkdtemp())/"replay.db"
    request={"taskId":task,"contextId":context_id,"idempotencyKey":"message-1","skillId":"account.lookup","message":{},"metadata":{}}
    body=json.dumps(request,separators=(",",":")).encode(); now=datetime.now(timezone.utc)
    context={"contractVersion":"light-a2a-backend/v1","invocationId":str(uuid.uuid4()),"issuer":"light-a2a","audience":"account-backend","hostId":str(uuid.uuid4()),"environment":"dev","principalSubject":"user:1","callerAgentRef":"caller","targetAgentRef":"account.agent","bindingId":str(uuid.uuid4()),"publicationId":str(uuid.uuid4()),"selectedSkillId":request["skillId"],"operation":operation,"taskId":task,"contextId":context_id,"idempotencyKey":"message-1","backendOperationId":"op-1" if operation in ("STATUS","CANCEL") else None,"policyDigest":"sha256:"+"a"*64,"dataBoundaryDigest":"sha256:"+"b"*64,"requestDigest":"sha256:"+hashlib.sha256(body).hexdigest(),"budget":{"maximumInputBytes":1024,"maximumOutputBytes":1024,"maximumArtifactBytes":1024},"traceparent":None,"issuedAt":now.isoformat(),"deadline":(now+timedelta(minutes=1)).isoformat(),"expiresAt":(now+timedelta(seconds=30)).isoformat()}
    digest="sha256:"+"c"*64
    config=AdapterConfig(key,digest,context["audience"],context["hostId"],context["environment"],context["targetAgentRef"],context["bindingId"],context["publicationId"],context["policyDigest"],context["dataBoundaryDigest"],ReplayStore(replay_path))
    return body,request,context,config,replay_path,signed_headers(body,context,key,digest)

class AdapterTest(unittest.TestCase):
    covered=set()

    @classmethod
    def tearDownClass(cls):
        report=os.environ.get("A2A_TCK_REPORT_DIR")
        if report:
            Path(report).mkdir(parents=True,exist_ok=True)
            (Path(report)/"python.json").write_text(json.dumps({"contract":"light-a2a-backend/v1","lightA2aBuildSha256":os.environ.get("LIGHT_A2A_BUILD_SHA256"),"coveredCases":sorted(cls.covered)}))

    def test_operations_and_restart_safe_replay(self):
        for operation,case in (("INVOKE","valid-unary"),("INVOKE_STREAM","valid-stream"),("STATUS","status"),("CANCEL","cancel")):
            body,_,context,config,_,headers=vector(operation)
            self.assertEqual(context["taskId"],verify(headers,body,operation,config)["taskId"]); self.covered.add(case)
        body,_,_,config,replay_path,headers=vector(); verify(headers,body,"INVOKE",config)
        restarted=replace(config,replay_store=ReplayStore(replay_path))
        with self.assertRaisesRegex(PermissionError,"replayed"): verify(headers,body,"INVOKE",restarted)
        self.covered.update({"replayed","restart-reconciliation"})

    def test_server_dispatches_stream_artifact_and_business_error(self):
        class Backend:
            def capabilities(self): return {"contractVersion":"light-a2a-backend/v1","streaming":True,"cancellation":True,"statusReconciliation":True,"acceptedContentModes":["application/json"],"maximumArtifactBytes":1024}
            def invoke(self, _context, request):
                if request["metadata"].get("fail"): raise ValueError("business rejected")
                return {"state":"COMPLETED","backendOperationId":"op-1","result":{},"error":None,"artifacts":[{"artifactId":str(uuid.uuid4()),"logicalName":"answer.txt","mediaType":"text/plain","contentBase64":"b2s=","contentDigest":"sha256:"+hashlib.sha256(b"ok").hexdigest(),"visibility":"OWNER"}]}
            def invoke_stream(self, _context, _request): return iter([{"sequenceNumber":1,"state":"COMPLETED","backendOperationId":"op-1","result":{},"error":None,"artifact":None,"terminal":True}])
            def status(self, _context, _request): return {"state":"WORKING","backendOperationId":"op-1","result":None,"error":None,"artifacts":[]}
            def cancel(self, _context, _request): return {"state":"CANCELED","backendOperationId":"op-1","result":None,"error":None,"artifacts":[]}
        with socket.socket() as listener:
            listener.bind(("127.0.0.1",0)); port=listener.getsockname()[1]
        body,request,context,config,_,_=vector(); server=serve(Backend(),config,port)
        try:
            def post(operation,path,request_value=request,context_value=context):
                payload=json.dumps(request_value,separators=(",",":")).encode(); context_value=dict(context_value); context_value["operation"]=operation; context_value["requestDigest"]="sha256:"+hashlib.sha256(payload).hexdigest(); context_value["invocationId"]=str(uuid.uuid4()); headers=signed_headers(payload,context_value,config.key,config.contract_digest)
                return urllib.request.urlopen(urllib.request.Request(f"http://127.0.0.1:{port}{path}",data=payload,headers=headers,method="POST"))
            with post("INVOKE","/v1/invoke") as response: self.assertEqual("answer.txt",json.load(response)["artifacts"][0]["logicalName"])
            with post("INVOKE_STREAM","/v1/invoke-stream") as response: self.assertIn(b'"terminal":true',response.read())
            failing=dict(request); failing["metadata"]={"fail":True}
            with self.assertRaises(urllib.error.HTTPError) as failure: post("INVOKE","/v1/invoke",failing,context)
            self.assertEqual(422,failure.exception.code)
        finally: server.shutdown(); server.server_close()
        self.covered.update({"artifact","business-error"})

    def test_rejection_matrix(self):
        config_mutations={
            "wrong-audience":("audience","wrong"), "wrong-host":("host_id",str(uuid.uuid4())),
            "wrong-environment":("environment","prod"), "wrong-agent":("target_agent_ref","wrong.agent"),
            "wrong-publication":("publication_id",str(uuid.uuid4())), "wrong-policy":("policy_digest","sha256:"+"d"*64),
            "wrong-data-boundary":("data_boundary_digest","sha256:"+"d"*64), "unconfigured-destination":("binding_id",str(uuid.uuid4())),
        }
        for case,(field,value) in config_mutations.items():
            body,_,_,config,_,headers=vector()
            with self.assertRaises(PermissionError): verify(headers,body,"INVOKE",replace(config,**{field:value}))
            self.covered.add(case)
        for case,field,value in (("wrong-skill","skillId","wrong.skill"),("wrong-task","taskId",str(uuid.uuid4())),("wrong-context","contextId",str(uuid.uuid4())),("wrong-idempotency","idempotencyKey","wrong")):
            body,request,context,config,_,_=vector(); request[field]=value; body=json.dumps(request,separators=(",",":")).encode(); headers=signed_headers(body,context,config.key,config.contract_digest)
            with self.assertRaises(PermissionError): verify(headers,body,"INVOKE",config)
            self.covered.add(case)
        for case,field,value in (("expired","expiresAt",(datetime.now(timezone.utc)-timedelta(seconds=1)).isoformat()),("deadline","deadline",(datetime.now(timezone.utc)-timedelta(seconds=1)).isoformat())):
            body,_,context,config,_,_=vector(); context[field]=value; headers=signed_headers(body,context,config.key,config.contract_digest)
            with self.assertRaises(PermissionError): verify(headers,body,"INVOKE",config)
            self.covered.add(case)
        body,_,context,config,_,_=vector(); headers=signed_headers(body,context,config.key,config.contract_digest)
        with self.assertRaises(PermissionError): verify(headers,body,"CANCEL",config)
        self.covered.add("wrong-operation")

    def test_signature_forgery(self):
        body,_,_,config,_,headers=vector()
        with self.assertRaisesRegex(PermissionError,"signature"): verify(headers,b"{}","INVOKE",config)
        self.covered.add("forged-signature")

    def test_z_manifest_is_fully_exercised(self): self.assertEqual(REQUIRED_CASES,self.covered)

if __name__=="__main__": unittest.main()
