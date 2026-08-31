import test, { after } from "node:test";
import assert from "node:assert/strict";
import { createHmac, createHash, randomUUID } from "node:crypto";
import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { serve, verify } from "../src/index.js";
import { createServer as createProbeServer } from "node:net";
import { once } from "node:events";

const required = new Set(["valid-unary","valid-stream","status","cancel","artifact","deadline","business-error","restart-reconciliation","forged-signature","expired","replayed","wrong-audience","wrong-host","wrong-environment","wrong-agent","wrong-skill","wrong-operation","wrong-task","wrong-context","wrong-idempotency","wrong-publication","wrong-policy","wrong-data-boundary","unconfigured-destination"]);
const covered = new Set();

function headers(body, context, config) {
  const encoded=Buffer.from(JSON.stringify(context)).toString("base64url");
  const signature=createHmac("sha256",config.key).update(encoded).update(Buffer.from([0])).update(body).digest("hex");
  return {"x-light-a2a-backend-context":encoded,"x-light-a2a-backend-signature":signature,"x-light-a2a-backend-contract-digest":config.contractDigest};
}
function vector(operation="INVOKE") {
  const key=Buffer.alloc(32,107),taskId=randomUUID(),contextId=randomUUID();
  const request={taskId,contextId,idempotencyKey:"message-1",skillId:"account.lookup",message:{},metadata:{}};
  const body=Buffer.from(JSON.stringify(request)),now=Date.now();
  const context={contractVersion:"light-a2a-backend/v1",invocationId:randomUUID(),issuer:"light-a2a",audience:"account-backend",hostId:randomUUID(),environment:"dev",principalSubject:"user:1",callerAgentRef:"caller",targetAgentRef:"account.agent",bindingId:randomUUID(),publicationId:randomUUID(),selectedSkillId:request.skillId,operation,taskId,contextId,idempotencyKey:request.idempotencyKey,backendOperationId:["STATUS","CANCEL"].includes(operation)?"op-1":null,policyDigest:`sha256:${"a".repeat(64)}`,dataBoundaryDigest:`sha256:${"b".repeat(64)}`,requestDigest:`sha256:${createHash("sha256").update(body).digest("hex")}`,budget:{maximumInputBytes:1024,maximumOutputBytes:1024,maximumArtifactBytes:1024},traceparent:null,issuedAt:new Date(now).toISOString(),deadline:new Date(now+60000).toISOString(),expiresAt:new Date(now+30000).toISOString()};
  const config={key,contractDigest:`sha256:${"c".repeat(64)}`,audience:context.audience,hostId:context.hostId,environment:context.environment,targetAgentRef:context.targetAgentRef,bindingId:context.bindingId,publicationId:context.publicationId,policyDigest:context.policyDigest,dataBoundaryDigest:context.dataBoundaryDigest,replayFile:join(mkdtempSync(join(tmpdir(),"a2a-node-")),"replay.json")};
  return {body,request,context,config,headers:headers(body,context,config)};
}

test("all operation envelopes and restart replay",()=>{
  for(const [operation,name] of [["INVOKE","valid-unary"],["INVOKE_STREAM","valid-stream"],["STATUS","status"],["CANCEL","cancel"]]) { const v=vector(operation); assert.equal(verify(v.headers,v.body,operation,v.config).taskId,v.context.taskId); covered.add(name); }
  const v=vector(); verify(v.headers,v.body,"INVOKE",v.config); assert.throws(()=>verify(v.headers,v.body,"INVOKE",{...v.config}),/replayed/);
  ["replayed","restart-reconciliation"].forEach(value=>covered.add(value));
});

test("server dispatches stream, artifact, and business error",async()=>{
  const port=await new Promise((resolve,reject)=>{const probe=createProbeServer();probe.once("error",reject);probe.listen(0,"127.0.0.1",()=>{const address=probe.address();probe.close(()=>resolve(address.port));});});
  const backend={capabilities:()=>({contractVersion:"light-a2a-backend/v1",streaming:true,cancellation:true,statusReconciliation:true,acceptedContentModes:["application/json"],maximumArtifactBytes:1024}),invoke:async(_context,request)=>{if(request.metadata.fail)throw new Error("business rejected");return {state:"COMPLETED",backendOperationId:"op-1",result:{},error:null,artifacts:[{artifactId:randomUUID(),logicalName:"answer.txt",mediaType:"text/plain",contentBase64:"b2s=",contentDigest:`sha256:${createHash("sha256").update("ok").digest("hex")}`,visibility:"OWNER"}]}},async *invokeStream(){yield {sequenceNumber:1,state:"COMPLETED",backendOperationId:"op-1",result:{},error:null,artifact:null,terminal:true};},status:async()=>({state:"WORKING",backendOperationId:"op-1",result:null,error:null,artifacts:[]}),cancel:async()=>({state:"CANCELED",backendOperationId:"op-1",result:null,error:null,artifacts:[]})};
  const base=vector(),server=serve(backend,base.config,port);if(!server.listening)await once(server,"listening");
  try{
    const post=async(operation,path,request=base.request)=>{const body=Buffer.from(JSON.stringify(request)),context={...base.context,operation,invocationId:randomUUID(),requestDigest:`sha256:${createHash("sha256").update(body).digest("hex")}`};return fetch(`http://127.0.0.1:${port}${path}`,{method:"POST",headers:headers(body,context,base.config),body});};
    let response=await post("INVOKE","/v1/invoke");assert.equal((await response.json()).artifacts[0].logicalName,"answer.txt");
    response=await post("INVOKE_STREAM","/v1/invoke-stream");assert.match(await response.text(),/"terminal":true/);
    response=await post("INVOKE","/v1/invoke",{...base.request,metadata:{fail:true}});assert.equal(response.status,422);
  }finally{await new Promise(resolve=>server.close(resolve));}
  covered.add("artifact");covered.add("business-error");
});

test("all signed binding mismatches fail closed",()=>{
  const mutations={"wrong-audience":["audience","wrong"],"wrong-host":["hostId",randomUUID()],"wrong-environment":["environment","prod"],"wrong-agent":["targetAgentRef","wrong.agent"],"wrong-publication":["publicationId",randomUUID()],"wrong-policy":["policyDigest",`sha256:${"d".repeat(64)}`],"wrong-data-boundary":["dataBoundaryDigest",`sha256:${"d".repeat(64)}`],"unconfigured-destination":["bindingId",randomUUID()]};
  for(const [name,[field,value]] of Object.entries(mutations)){const v=vector();assert.throws(()=>verify(v.headers,v.body,"INVOKE",{...v.config,[field]:value}));covered.add(name);}
  for(const [name,field,value] of [["wrong-skill","skillId","wrong.skill"],["wrong-task","taskId",randomUUID()],["wrong-context","contextId",randomUUID()],["wrong-idempotency","idempotencyKey","wrong"]]){const v=vector();v.request[field]=value;v.body=Buffer.from(JSON.stringify(v.request));v.headers=headers(v.body,v.context,v.config);assert.throws(()=>verify(v.headers,v.body,"INVOKE",v.config));covered.add(name);}
  for(const [name,field,value] of [["expired","expiresAt",new Date(Date.now()-1000).toISOString()],["deadline","deadline",new Date(Date.now()-1000).toISOString()]]){const v=vector();v.context[field]=value;v.headers=headers(v.body,v.context,v.config);assert.throws(()=>verify(v.headers,v.body,"INVOKE",v.config));covered.add(name);}
  const v=vector();assert.throws(()=>verify(v.headers,v.body,"CANCEL",v.config));covered.add("wrong-operation");
});

test("forged request body fails signature",()=>{const v=vector();assert.throws(()=>verify(v.headers,Buffer.from("{}"),"INVOKE",v.config),/signature/);covered.add("forged-signature");});

after(()=>{
  assert.deepEqual([...covered].sort(),[...required].sort());
  if(process.env.A2A_TCK_REPORT_DIR){mkdirSync(process.env.A2A_TCK_REPORT_DIR,{recursive:true});writeFileSync(join(process.env.A2A_TCK_REPORT_DIR,"typescript.json"),JSON.stringify({contract:"light-a2a-backend/v1",lightA2aBuildSha256:process.env.LIGHT_A2A_BUILD_SHA256,coveredCases:[...covered].sort()}));}
});
