import fs from 'node:fs';
import assert from 'node:assert/strict';
import Ajv from 'ajv/dist/2020.js';
import formats from 'ajv-formats';
const load=name=>JSON.parse(fs.readFileSync(new URL(name,import.meta.url),'utf8'));
const schemas=load('schemas.json');const catalog=load('workflow-tools-list-full.json');
const ajv=new Ajv({strict:true,allowUnionTypes:true});formats(ajv);ajv.addSchema(schemas,'schemas.json');
const tool=catalog.tools.find(t=>t.name==='workflow_binding_get');
const input={hostId:'01964b05-552a-7c4b-9184-6857e7f3dc5f',toolId:'019ffba3-fcee-7dc6-bac1-d50dba320517',wfDefId:'01a01b8b-3e17-775c-935e-95b4f3cb17a3',headOnly:true};
const output={interfaceVersion:'workflow-publication-head-v1',hostId:input.hostId,toolId:input.toolId,wfDefId:input.wfDefId,exists:false,aggregateVersion:0};
let checks=0;
for(const schema of [tool.inputSchema,tool.contractInputSchema]) {
 const validate=ajv.compile(schema);assert.ok(validate(input));checks++;
 const missing={...input};delete missing.wfDefId;assert.equal(validate(missing),false);checks++;
 assert.equal(validate({...input,bindingId:input.toolId}),false);checks++;
 assert.equal(validate({...input,headOnly:false}),false);checks++;
}
for(const schema of [tool.outputSchema,tool.contractOutputSchema]) {
 const validate=ajv.compile(schema);assert.ok(validate(output));checks++;
 for(const [key,value] of [['interfaceVersion','wrong'],['aggregateVersion',-1],['aggregateVersion',0.5],['exists','false']]) {
  assert.equal(validate({...output,[key]:value}),false);checks++;
 }
}
console.log(`publication head contract: ${checks} checks passed`);
