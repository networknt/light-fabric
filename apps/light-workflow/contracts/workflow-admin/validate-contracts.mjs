import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import Ajv2020 from 'ajv/dist/2020.js';
import addFormats from 'ajv-formats';

const root = path.dirname(fileURLToPath(import.meta.url));
const read = (name) => JSON.parse(fs.readFileSync(path.join(root, name), 'utf8'));
const manifest = read('tool-manifest.json');
const schemas = read('schemas.json');
const examples = read('examples.json');
const errors = read('errors.json');
const failures = [];
const fail = (message) => failures.push(message);

const expected = [
  'workflow_list_processes','workflow_get_process','workflow_list_features','workflow_get_feature',
  'workflow_get_status','workflow_get_result','workflow_cancel','workflow_cancel_feature',
  'workflow_get_human_task_inbox_summary','workflow_list_human_tasks','workflow_get_human_task',
  'workflow_claim_human_task','workflow_release_human_task','workflow_complete_human_task'
];
const names = manifest.tools.map((tool) => tool.name);
if (new Set(names).size !== names.length) fail('tool names must be unique');
for (const name of expected) if (!names.includes(name)) fail(`missing tool ${name}`);
for (const name of names) if (!examples[name]) fail(`missing examples for ${name}`);
for (const name of Object.keys(examples)) if (!names.includes(name)) fail(`orphan examples for ${name}`);
if (manifest.protocolTarget !== '2026-07-28') fail('protocolTarget must be 2026-07-28');
if (manifest.identitySource !== 'trustedInvocationContext') fail('identity must come from trusted invocation context');

const resolve = (ref) => {
  const [file, pointer = ''] = ref.split('#');
  let value = file ? read(file) : schemas;
  for (const part of pointer.replace(/^\//, '').split('/').filter(Boolean)) value = value?.[part.replaceAll('~1','/').replaceAll('~0','~')];
  return value;
};
const ajv = new Ajv2020({allErrors:true, strict:true, allowUnionTypes:true});
addFormats(ajv);
ajv.addSchema(schemas, 'schemas.json');
const validateExample = (value, schema, at) => {
  try {
    const validate = ajv.compile(schema);
    if (!validate(value)) {
      for (const error of validate.errors || []) fail(`${at}${error.instancePath}: ${error.message}`);
    }
  } catch (error) {
    fail(`${at}: schema compilation failed: ${error.message}`);
  }
};
const forbiddenIdentity = /^(?:host_?id|owner(?:_?subject)?|roles?|authorization|vm_?generation)$/i;
for (const spelling of ['hostId','host_id','owner','ownerSubject','owner_subject','role','roles','authorization','vmGeneration','vm_generation']) {
  if (!forbiddenIdentity.test(spelling)) fail(`identity guard does not cover ${spelling}`);
}
function scanResolvedInput(schema, at, seen = new Set()) {
  if (!schema || typeof schema !== 'object') return;
  if (schema.$ref) {
    if (seen.has(schema.$ref)) return;
    const resolved = resolve(schema.$ref);
    if (!resolved) return fail(`${at}: unresolved schema reference ${schema.$ref}`);
    scanResolvedInput(resolved, `${at}->${schema.$ref}`, new Set([...seen, schema.$ref]));
  }
  for (const [name, child] of Object.entries(schema.properties || {})) {
    if (forbiddenIdentity.test(name)) fail(`${at}.${name}: untrusted identity/fencing argument exposed`);
    scanResolvedInput(child, `${at}.${name}`, seen);
  }
  for (const keyword of ['allOf','anyOf','oneOf','prefixItems']) for (const child of schema[keyword] || []) scanResolvedInput(child, at, seen);
  if (schema.items) scanResolvedInput(schema.items, `${at}[]`, seen);
  if (schema.additionalProperties && typeof schema.additionalProperties === 'object') scanResolvedInput(schema.additionalProperties, `${at}.*`, seen);
}

for (const tool of manifest.tools) {
  if (!tool.permission || typeof tool.sideEffect !== 'boolean') fail(`${tool.name}: permission/sideEffect missing`);
  validateExample(examples[tool.name]?.input, tool.inputSchema, `${tool.name}.input`);
  validateExample(examples[tool.name]?.output, tool.outputSchema, `${tool.name}.output`);
  scanResolvedInput(tool.inputSchema, `${tool.name}.inputSchema`);
}
const listPage = schemas.$defs.PageInput.properties.pageSize;
if (listPage.default !== 25 || listPage.maximum !== 100) fail('pagination must default to 25 and cap at 100');
if (examples.workflow_list_processes.output.processes[0].workflowInstanceId !== null) fail('process-only fixture must retain null workflowInstanceId');
if (examples.workflow_get_human_task.output.task.taskId === examples.workflow_get_human_task.output.task.taskAsstId) fail('taskId and taskAsstId must remain distinct');
for (const code of ['STORE_UNAVAILABLE','AUTHORITY_UNAVAILABLE','VERSION_CONFLICT','CLAIM_CONFLICT','VALIDATION_FAILED','TASK_EXPIRED']) if (!errors.errors.some((error) => error.code === code)) fail(`missing stable error ${code}`);

if (failures.length) {
  console.error(failures.map((failure) => `FAIL ${failure}`).join('\n'));
  process.exit(1);
}
console.log(`PASS workflow-admin contracts: ${manifest.tools.length} tools, ${errors.errors.length} stable errors, ${Object.keys(examples).length} example pairs`);
