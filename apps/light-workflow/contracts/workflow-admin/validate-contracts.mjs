import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import Ajv2020 from 'ajv/dist/2020.js';
import addFormats from 'ajv-formats';

const root = path.dirname(fileURLToPath(import.meta.url));
const read = (name) => JSON.parse(fs.readFileSync(path.join(root, name), 'utf8'));
const catalog = read('workflow-tools-list-full.json');
const manifest = {...catalog, tools: catalog.tools.map(tool => ({
  ...tool,
  inputSchema: tool.contractInputSchema || tool.inputSchema,
  outputSchema: tool.contractOutputSchema || tool.outputSchema,
}))};
const schemas = read('schemas.json');
const examples = read('examples.json');
const errors = read('errors.json');
const gateway = read('gateway-publication.json');
const failures = [];
const fail = (message) => failures.push(message);

const expected = [
  'workflow_decide_tool_access',
  'workflow_delete_process','workflow_get_task','workflow_add_process_note','workflow_list_process_notes',
  'workflow_list_processes','workflow_get_process','workflow_list_features','workflow_get_feature',
  'workflow_get_status','workflow_get_result','workflow_cancel','workflow_cancel_feature',
  'workflow_get_human_task_inbox_summary','workflow_list_human_tasks','workflow_get_human_task',
  'workflow_claim_human_task','workflow_release_human_task','workflow_complete_human_task',
  'workflow_definition_save','workflow_definition_publish','workflow_definition_retire',
  'workflow_definition_grants_sync','workflow_binding_publish','workflow_binding_retire',
  'workflow_binding_get','workflow_binding_list','workflow_binding_decide','workflow_binding_revoke','workflow_invoke'
];
const names = manifest.tools.map((tool) => tool.name);
if (new Set(names).size !== names.length) fail('tool names must be unique');
if (catalog.tools.length !== 33 || catalog.tools.find(tool => tool.name === 'workflow_invoke')?.gatewayPublication !== false) {
  fail('the Workflow MCP catalog must include all 33 Tools and exclude workflow_invoke from Gateway publication');
}
for (const name of expected) if (!names.includes(name)) fail(`missing tool ${name}`);
for (const name of names) if (!examples[name]) fail(`missing examples for ${name}`);
for (const name of Object.keys(examples)) if (!names.includes(name)) fail(`orphan examples for ${name}`);
if (manifest.protocolTarget !== '2026-07-28') fail('protocolTarget must be 2026-07-28');
if (manifest.identitySource !== 'trustedInvocationContext') fail('identity must come from trusted invocation context');
for (const [name, value] of [['workflow_definition_save','authorization'],['workflow_definition_publish','header'],['workflow_definition_retire','header'],['workflow_definition_grants_sync','authorization'],['workflow_binding_publish','header'],['workflow_binding_retire','header']]) {
  if (manifest.tools.find(tool => tool.name === name)?.publisherToken !== value) fail(`${name}: publisherToken metadata must be ${value}`);
}

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
const hostTools = new Set(['workflow_definition_save','workflow_definition_publish','workflow_definition_retire',
  'workflow_definition_grants_sync','workflow_binding_publish','workflow_binding_retire','workflow_binding_get',
  'workflow_binding_list','workflow_binding_decide','workflow_binding_revoke']);
const allowedIdentityPath = (tool, path) =>
  (path === '/hostId' && hostTools.has(tool))
  || (path === '/owner' && ['workflow_definition_save','workflow_definition_publish'].includes(tool))
  || (path === '/role' && tool === 'workflow_binding_list');
function scanResolvedInput(schema, toolName, propertyPath = '', at = toolName, seen = new Set()) {
  if (!schema || typeof schema !== 'object') return [];
  const violations = [];
  if (schema.$ref) {
    if (seen.has(schema.$ref)) return violations;
    const resolved = resolve(schema.$ref);
    if (!resolved) return [`${at}: unresolved schema reference ${schema.$ref}`];
    violations.push(...scanResolvedInput(resolved, toolName, propertyPath, `${at}->${schema.$ref}`, new Set([...seen, schema.$ref])));
  }
  for (const [name, child] of Object.entries(schema.properties || {})) {
    const childPath = `${propertyPath}/${name}`;
    if (forbiddenIdentity.test(name) && !allowedIdentityPath(toolName, childPath)) violations.push(`${at}.${name}: untrusted identity/fencing argument exposed`);
    violations.push(...scanResolvedInput(child, toolName, childPath, `${at}.${name}`, seen));
  }
  for (const keyword of ['allOf','anyOf','oneOf','prefixItems']) for (const child of schema[keyword] || []) violations.push(...scanResolvedInput(child, toolName, propertyPath, at, seen));
  if (schema.items) violations.push(...scanResolvedInput(schema.items, toolName, `${propertyPath}[]`, `${at}[]`, seen));
  if (schema.additionalProperties && typeof schema.additionalProperties === 'object') violations.push(...scanResolvedInput(schema.additionalProperties, toolName, `${propertyPath}.*`, `${at}.*`, seen));
  return violations;
}

for (const tool of manifest.tools) {
  if (!tool.permission || typeof tool.sideEffect !== 'boolean') fail(`${tool.name}: permission/sideEffect missing`);
  if (tool.publisherToken && !['header','authorization'].includes(tool.publisherToken)) fail(`${tool.name}: invalid publisherToken metadata`);
  if (tool.gatewayPublication === false && tool.name !== 'workflow_invoke') fail(`${tool.name}: only workflow_invoke may be excluded from Gateway publication`);
  validateExample(examples[tool.name]?.input, tool.inputSchema, `${tool.name}.input`);
  validateExample(examples[tool.name]?.output, tool.outputSchema, `${tool.name}.output`);
  if (examples[tool.name]?.errorResult) validateExample(examples[tool.name].errorResult, { $ref:'schemas.json#/$defs/WorkflowErrorResult' }, `${tool.name}.errorResult`);
  for (const violation of scanResolvedInput(tool.inputSchema, tool.name, '', `${tool.name}.inputSchema`)) fail(violation);
}
const startInputSchema = manifest.tools.find(tool => tool.name === 'workflow_start')?.inputSchema;
const startInput = examples.workflow_start.input;
validateExample({...startInput, expectedDefinitionDigest: `sha256:${'a'.repeat(64)}`}, startInputSchema,
  'workflow_start.fencedInput');
const invalidStartDigest = ajv.compile(startInputSchema);
if (invalidStartDigest({...startInput, expectedDefinitionDigest: 'invalid'})) {
  fail('workflow_start must reject a malformed expectedDefinitionDigest');
}
for (const state of ['failed', 'cancelled']) {
  const value = examples.workflow_wait_result[`${state}Error`];
  validateExample(value, {$ref:'schemas.json#/$defs/WorkflowErrorResult'}, `workflow_wait_result.${state}Error`);
}
for (const toolName of hostTools) {
  const tool = manifest.tools.find(item => item.name === toolName);
  const input = tool && resolve(tool.inputSchema.$ref);
  if (!input?.required?.includes('hostId') || input?.properties?.hostId?.$ref !== '#/$defs/Uuid') fail(`${toolName}:/hostId must be a required UUID assertion`);
}
for (const toolName of ['workflow_definition_save','workflow_definition_publish']) {
  const input = resolve(manifest.tools.find(item => item.name === toolName).inputSchema.$ref);
  const ownerSchema = input.properties.owner?.$ref ? resolve(input.properties.owner.$ref) : input.properties.owner;
  if (ownerSchema?.type !== 'object' || ownerSchema.additionalProperties !== false || (ownerSchema.required || []).length
      || JSON.stringify(Object.keys(ownerSchema.properties || {}).sort()) !== JSON.stringify(['positionId','userId'])
      || ownerSchema.properties.userId?.$ref !== '#/$defs/Uuid'
      || ownerSchema.properties.positionId?.type !== 'string' || ownerSchema.properties.positionId?.minLength !== 1
      || ownerSchema.properties.positionId?.maxLength !== 128) fail(`${toolName}:/owner must remain constrained resource-owner metadata`);
}
const bindingListInput = resolve(manifest.tools.find(item => item.name === 'workflow_binding_list').inputSchema.$ref);
if (!bindingListInput.required?.includes('role') || JSON.stringify(bindingListInput.properties.role?.enum) !== JSON.stringify(['owner','requester'])) fail('workflow_binding_list:/role must remain the owner/requester relationship filter');
const expectRejected = (tool, schema, description) => { if (!scanResolvedInput(schema, tool).length) fail(`validator negative test accepted ${description}`); };
expectRejected('workflow_invoke', {type:'object',properties:{hostId:{type:'string'}}}, 'hostId on workflow_invoke');
expectRejected('workflow_start', {type:'object',properties:{hostId:{type:'string'}}}, 'hostId on existing workflow_start');
expectRejected('workflow_definition_save', {type:'object',properties:{host_id:{type:'string'}}}, 'host_id alias');
expectRejected('workflow_definition_save', {type:'object',properties:{nested:{type:'object',properties:{hostId:{type:'string'}}}}}, 'nested hostId');
expectRejected('workflow_definition_save', {type:'object',properties:{owner:{type:'object',properties:{roles:{type:'array'}}}}}, 'nested owner identity');
schemas.$defs.ValidatorIdentityProbe = {type:'object',properties:{nested:{type:'object',properties:{hostId:{type:'string'}}}}};
expectRejected('workflow_binding_get', {$ref:'#/$defs/ValidatorIdentityProbe'}, 'nested hostId through a referenced schema');
const operationIdTools = ['workflow_definition_publish','workflow_definition_retire','workflow_binding_publish',
  'workflow_binding_retire','workflow_binding_decide','workflow_binding_revoke'];
for (const toolName of operationIdTools) {
  const tool = manifest.tools.find(item => item.name === toolName);
  const inputSchema = resolve(tool.inputSchema.$ref);
  if (inputSchema.properties.operationId?.$ref !== '#/$defs/Uuid') fail(`${toolName}.operationId must use the UUID contract`);
  const validate = ajv.compile(tool.inputSchema);
  const malformed = structuredClone(examples[toolName].input);
  malformed.operationId = 'not-a-uuid';
  if (validate(malformed)) fail(`validator accepted malformed ${toolName}.operationId`);
}
if (schemas.$defs.BindingDependency?.properties?.dispatchTarget?.type !== 'object') fail('BindingDependency.dispatchTarget must remain an object');
const bindingPublishTool = manifest.tools.find(item => item.name === 'workflow_binding_publish');
const validatePublishOutput = ajv.compile(bindingPublishTool.outputSchema);
const activePublishReceipt = examples.workflow_binding_publish.output;
const pendingPublishReceipt = examples.workflow_binding_publish.pendingOutput;
if (activePublishReceipt.status !== 'active' || Object.hasOwn(activePublishReceipt, 'carryOverDeniedReason')
    || !validatePublishOutput(activePublishReceipt)) fail('active binding publish receipt must validate without a carry-over denial reason');
if (pendingPublishReceipt.status !== 'pendingApproval' || typeof pendingPublishReceipt.carryOverDeniedReason !== 'string'
    || !validatePublishOutput(pendingPublishReceipt)) fail('pending binding publish receipt must validate with a carry-over denial reason');
const unexpectedPublishField = {...pendingPublishReceipt, unexpectedField: true};
if (validatePublishOutput(unexpectedPublishField)) fail('binding publish receipt must reject additional properties');
const badPublishDependency = structuredClone(examples.workflow_binding_publish.input);
badPublishDependency.dependencies[0].dispatchTarget = 'claims.lookup@call';
if (ajv.compile(bindingPublishTool.inputSchema)(badPublishDependency)) fail('validator accepted string dispatchTarget on binding publish');
const bindingGetTool = manifest.tools.find(item => item.name === 'workflow_binding_get');
const badReadDependency = structuredClone(examples.workflow_binding_get.output);
badReadDependency.revision.dependencies[0].dispatchTarget = 'claims.lookup@call';
if (ajv.compile(bindingGetTool.outputSchema)(badReadDependency)) fail('validator accepted string dispatchTarget in binding read');
for (const toolName of ['workflow_definition_save','workflow_definition_publish']) {
  const tool = manifest.tools.find(item => item.name === toolName);
  const validate = ajv.compile(tool.inputSchema);
  const validPosition = structuredClone(examples[toolName].input);
  validPosition.owner = {positionId:'workflow-admin'};
  if (!validate(validPosition)) fail(`validator rejected varchar positionId for ${toolName}`);
  const longPosition = structuredClone(validPosition);
  longPosition.owner.positionId = 'p'.repeat(129);
  if (validate(longPosition)) fail(`validator accepted overlong positionId for ${toolName}`);
}
const nativeNames = ['workflow_start', 'workflow_decide_tool_access', 'workflow_delete_process',
  'workflow_get_task', 'workflow_add_process_note', 'workflow_list_process_notes',
  'workflow_definition_save','workflow_definition_publish','workflow_definition_retire','workflow_definition_grants_sync',
  'workflow_binding_publish','workflow_binding_retire','workflow_binding_get','workflow_binding_list',
  'workflow_binding_decide','workflow_binding_revoke'];
const internalPublicationViolation = (published, contracts) => published
  .filter(item => contracts.find(tool => tool.name === item.name)?.gatewayPublication === false)
  .map(item => item.name);
if (internalPublicationViolation(gateway.tools, manifest.tools).length) fail('Gateway publication contains an excluded Tool');
if (!internalPublicationViolation([{name:'workflow_invoke'}], [{name:'workflow_invoke',gatewayPublication:false}]).includes('workflow_invoke')) {
  fail('validator negative test did not reject workflow_invoke in Gateway publication');
}
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
  if (contract?.gatewayPublication === false) fail(`${published.name}: Tool cannot be published on Gateway`);
  if (published.permission !== contract?.permission || published.endpoint !== `${published.name}@call`) {
    fail(`${published.name}: Gateway publication permission or endpoint differs from Workflow contract`);
  }
}
function hasReference(value) {
  if (!value || typeof value !== 'object') return false;
  if (Array.isArray(value)) return value.some(hasReference);
  return Object.hasOwn(value, '$ref') || Object.values(value).some(hasReference);
}
for (const tool of catalog.tools) {
  if (hasReference(tool.inputSchema) || hasReference(tool.outputSchema)) {
    fail(`${tool.name}: public Tool schema contains an unresolved reference`);
  }
  validateExample(examples[tool.name]?.input, tool.inputSchema, `${tool.name}.publicInput`);
  validateExample(examples[tool.name]?.output, tool.outputSchema, `${tool.name}.publicOutput`);
}
for (const published of gateway.tools) {
  if (!catalog.tools.some(tool => tool.name === published.name)) {
    fail(`${published.name}: Gateway publication is absent from the public Tool list`);
  }
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
for (const code of ['STORE_UNAVAILABLE','AUTHORITY_UNAVAILABLE','VERSION_CONFLICT','CLAIM_CONFLICT','VALIDATION_FAILED','TASK_EXPIRED','PROCESS_NOT_TERMINAL','RESOURCE_HELD','IDEMPOTENCY_CONFLICT',
  'WORKFLOW_CAPACITY_EXHAUSTED','WORKFLOW_POLICY_DENIED','WORKFLOW_DEFINITION_MISMATCH','WORKFLOW_DEFINITION_RETIRED','WORKFLOW_BINDING_LIMIT_EXCEEDED','WORKFLOW_TIMEOUT','WORKFLOW_IDEMPOTENCY_CONFLICT','WORKFLOW_START_REJECTED','WORKFLOW_INPUT_INVALID','WORKFLOW_TASK_FAILED','WORKFLOW_OUTPUT_INVALID','WORKFLOW_BUDGET_EXHAUSTED','WORKFLOW_CANCELLED']) {
  if (!errors.errors.some((error) => error.code === code)) fail(`missing stable error ${code}`);
}
if (errors.errors.some(error => error.code === 'WORKFLOW_BINDING_PENDING')) fail('WORKFLOW_BINDING_PENDING is not an approved error');

if (failures.length) {
  console.error(failures.map((failure) => `FAIL ${failure}`).join('\n'));
  process.exit(1);
}
console.log(`PASS workflow-admin contracts: ${catalog.tools.length} MCP tools, ${errors.errors.length} stable errors, ${Object.keys(examples).length} example pairs`);
