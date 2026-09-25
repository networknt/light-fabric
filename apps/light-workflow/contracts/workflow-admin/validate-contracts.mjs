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
const gateway = read('gateway-publication.json');
const gatewayTools = read('gateway-tools-list.json');
const failures = [];
const fail = (message) => failures.push(message);

const expected = [
  'workflow_decide_tool_access',
  'workflow_delete_process','workflow_get_task','workflow_add_process_note','workflow_list_process_notes',
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
const nativeNames = ['workflow_start', 'workflow_decide_tool_access', 'workflow_delete_process',
  'workflow_get_task', 'workflow_add_process_note', 'workflow_list_process_notes'];
if (gateway.serviceId !== 'com.networknt.workflow-1.0.0' || gateway.path !== '/mcp'
    || gateway.apiType !== 'mcp' || gateway.backendMcpProtocol !== 'stateless'
    || gateway.backendCredentialMode !== 'workflow' || gateway.sessionIndependent !== true) {
  fail('Gateway native Workflow profile is invalid');
}
if (JSON.stringify(gateway.tools.map(tool => tool.name)) !== JSON.stringify(nativeNames)) {
  fail('Gateway additive publication names must match implemented native Tools');
}
for (const published of gateway.tools) {
  const contract = manifest.tools.find(tool => tool.name === published.name);
  if (published.permission !== contract?.permission || published.endpoint !== `${published.name}@call`) {
    fail(`${published.name}: Gateway publication permission or endpoint differs from Workflow contract`);
  }
}
if (JSON.stringify(gatewayTools.tools.map(tool => tool.name)) !== JSON.stringify(nativeNames)) {
  fail('Gateway import spec must contain exactly the additive native Tools');
}
function hasReference(value) {
  if (!value || typeof value !== 'object') return false;
  if (Array.isArray(value)) return value.some(hasReference);
  return Object.hasOwn(value, '$ref') || Object.values(value).some(hasReference);
}
for (const tool of gatewayTools.tools) {
  if (hasReference(tool.inputSchema) || hasReference(tool.outputSchema)) {
    fail(`${tool.name}: Gateway import schema contains an unresolved reference`);
  }
  validateExample(examples[tool.name]?.input, tool.inputSchema, `${tool.name}.gatewayInput`);
  validateExample(examples[tool.name]?.output, tool.outputSchema, `${tool.name}.gatewayOutput`);
}
if (!gateway.restrictedRoutes.every(route => ['GET','POST'].includes(route.method) && route.path.startsWith('/')
    && ['light-oauth','portal-bff-loc'].includes(route.target) && route.authentication)) {
  fail('Gateway restricted route inventory is incomplete or malformed');
}
if (new Set(gateway.restrictedRoutes.map(route => `${route.method} ${route.path}`)).size !== gateway.restrictedRoutes.length) {
  fail('Gateway restricted routes must be unique');
}
if (gateway.restrictedRoutes.some(route => route.path.includes('current-roles'))) {
  fail('current-role issuer route is outside the Gateway ACL task profile');
}
if (!gateway.restrictedRoutes.some(route => route.method === 'GET' && route.path === '/portal/query')
    || !gateway.restrictedRoutes.some(route => route.method === 'POST' && route.path === '/portal/command')) {
  fail('approval service routes must match the Workflow client methods');
}
const listPage = schemas.$defs.PageInput.properties.pageSize;
if (listPage.default !== 25 || listPage.maximum !== 100) fail('pagination must default to 25 and cap at 100');
if (examples.workflow_list_processes.output.processes[0].workflowInstanceId !== null) fail('process-only fixture must retain null workflowInstanceId');
if (examples.workflow_get_human_task.output.task.taskId === examples.workflow_get_human_task.output.task.taskAsstId) fail('taskId and taskAsstId must remain distinct');
for (const code of ['STORE_UNAVAILABLE','AUTHORITY_UNAVAILABLE','VERSION_CONFLICT','CLAIM_CONFLICT','VALIDATION_FAILED','TASK_EXPIRED','PROCESS_NOT_TERMINAL','RESOURCE_HELD','IDEMPOTENCY_CONFLICT']) if (!errors.errors.some((error) => error.code === code)) fail(`missing stable error ${code}`);

if (failures.length) {
  console.error(failures.map((failure) => `FAIL ${failure}`).join('\n'));
  process.exit(1);
}
console.log(`PASS workflow-admin contracts: ${manifest.tools.length} tools, ${errors.errors.length} stable errors, ${Object.keys(examples).length} example pairs`);
