import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import Ajv2020 from '../node_modules/ajv/dist/2020.js';
import addFormats from '../node_modules/ajv-formats/dist/index.js';

const root = path.dirname(fileURLToPath(import.meta.url));
const read = (name) => JSON.parse(fs.readFileSync(path.join(root, name), 'utf8'));
const contract = read('contracts.json');
const schemas = read('schemas.json');
const examples = read('examples.json');
const retirement = read('retirement.json');
const coverage = read('fixture-coverage.json');
const fail = (message) => { throw new Error(message); };
const assert = (condition, message) => { if (!condition) fail(message); };

const ajv = new Ajv2020({allErrors:true, strict:true});
addFormats(ajv);
const validators = Object.fromEntries(Object.keys(schemas.$defs).map(name => [name, ajv.compile({$schema:schemas.$schema, $defs:schemas.$defs, $ref:`#/$defs/${name}`})]));
function validate(name, value, shouldPass = true) {
  const valid = validators[name]?.(value);
  assert(valid === shouldPass, `${name}: expected ${shouldPass ? 'valid' : 'invalid'}; ${JSON.stringify(validators[name]?.errors)}`);
}
assert(contract.contractVersion === '1.0.0-proposed', 'contract version changed');
assert(contract.protocolTarget === '2026-07-28', 'protocol target changed');
assert(contract.publicIngress === 'Gateway POST /mcp', 'public ingress changed');
const expectedTools = ['workflow_start','workflow_decide_tool_access','workflow_delete_process','workflow_get_task','workflow_add_process_note','workflow_list_process_notes','workflow_list_audit'];
assert(JSON.stringify(contract.tools.map(t => t.name)) === JSON.stringify(expectedTools), 'new native tool roster changed');
assert(new Set([...contract.tools,...contract.serviceOperations].map(t => t.name)).size === 10, 'duplicate operation name');
assert(contract.tools.find(t => t.name === 'workflow_list_audit')?.ownerStep === 15, 'audit must retain its separate Step 15 gate');
assert(contract.existingRoleTools.length === 6, 'all six existing human task tools must receive ROLE support');
for (const operation of [...contract.tools,...contract.serviceOperations]) {
  assert(operation.ownerStep > 1, `${operation.name}: Step 01 cannot advertise implementation`);
  if ('permission' in operation) assert(Boolean(operation.permission), `${operation.name}: missing Gateway permission`);
  assert(examples[operation.name], `${operation.name}: missing example`);
  validate(operation.input, examples[operation.name].input);
  validate(operation.output, examples[operation.name].output);
}
for (const operation of contract.serviceOperations) assert(operation.serviceId && operation.audience && operation.scope, `${operation.name}: service authorization missing`);
assert(contract.serviceOperations[0].name === 'getCurrentWorkflowRoles' && contract.serviceOperations[0].scope === 'portal.r', 'role authority contract changed');
assert(contract.serviceOperations.slice(1).every(op => op.serviceId === 'light-workflow-approval' && op.audience === 'portal-workflow-approval'), 'approval service identity changed');
for (const name of contract.internalContracts) validate(name, examples[name]);
assert(Object.keys(examples).length === contract.tools.length + contract.serviceOperations.length + contract.internalContracts.length, 'orphan example');

const forbidden = ['hostId','host_id','owner','ownerSubject','roles','role','authorization','authorityExpiresAt','budgetVersion','profile','parentActionId','claimedBy','actorSubject','createdAt','status'];
for (const tool of contract.tools) {
  const fields = Object.keys(schemas.$defs[tool.input].properties ?? {});
  for (const field of forbidden) assert(!fields.includes(field), `${tool.name}: caller authority field ${field}`);
  validate(tool.input, {...examples[tool.name].input, hostId:'00000000-0000-4000-8000-000000000015'}, false);
}
validate('StartInput', {...examples.workflow_start.input, expectedDefinitionDigest:'not-a-digest'}, false);
validate('StartInput', {...examples.workflow_start.input, approvalRequestId:'00000000-0000-4000-8000-000000000007'}, false);
validate('StartReceipt', {...examples.workflow_start.output, accepted:false}, false);
validate('DecisionReceipt', {...examples.workflow_decide_tool_access.output, state:'GRANTED'}, false);
validate('NativeDeleteReceipt', {...examples.workflow_delete_process.output, logicalDeletion:'CLAIMED'}, false);
validate('RoleMembershipDecision', {...examples.RoleMembershipDecision, current:'yes'}, false);
validate('RoleMembershipSnapshot', {...examples.getCurrentWorkflowRoles.output, currentRoleIds:['genai-admin','genai-admin']}, false);
validate('ApprovalServiceEvidence', {...examples.ApprovalServiceEvidence, audience:'public'}, false);

const expected = {
  query:['getProcessInfo','getFreshProcessInfo','getProcessInfoLabel','getTaskInfo','getFreshTaskInfo','getTaskInfoLabel','getTaskAsst','getFreshTaskAsst','getHumanTaskList','getHumanTask','getHumanTaskInboxSummary','getAuditLog','getWorkflowEventFailure'],
  conditionalStaleQuery:['getFreshAuditLog','getAuditLogLabel'],
  command:['startWorkflow','completeTask','claimHumanTask','releaseHumanTask','createProcessInfo','updateProcessInfo','deleteProcessInfo','createTaskInfo','updateTaskInfo','deleteTaskInfo','createTaskAsst','updateTaskAsst','deleteTaskAsst','createAuditLog'],
  conditionalStaleCommand:['updateAuditLog','deleteAuditLog'],
  replaceContract:['requestWorkflowToolAccess'],
  restrictCommand:['decideWorkflowToolAccess'],
  retainControlPlane:['getWorklist','getFreshWorklist','getWorklistLabel','WorklistColumn','definition/access-request queries']
};
for (const [category, names] of Object.entries(expected)) assert(JSON.stringify(retirement[category]) === JSON.stringify(names), `${category}: exact inventory names changed`);
const keys = Object.entries(expected).flatMap(([category]) => retirement[category].map(name => `${category}/${name}`));
assert(new Set(keys).size === keys.length, 'duplicate retirement key');
assert(retirement.ownerStep === 13, 'retirement cannot happen in Step 01');
for (const code of ['AUTHORITY_UNAVAILABLE','ROLE_NOT_CURRENT','BINDING_NOT_READY','ACCEPTANCE_UNCONFIRMED','DECISION_PENDING_DELIVERY','PROCESS_NOT_TERMINAL']) assert(contract.errors[code], `missing error ${code}`);
const workspace = path.resolve(root, '../../../../../..');
const requiredCases = ['parity-insurance-rest','parity-insurance-mcp','parity-insurance-headless','parity-run-shell','role-human-approval','role-user-ask','role-legacy-ask','approval-role-workflow','approval-crash-before-start','approval-crash-link-ack','approval-crash-portal-commit','approval-crash-workflow-ack','approval-stale-cancel-race','legacy-deleted-publication','audit-separate-gate'];
assert(JSON.stringify(coverage.cases.map(c => c.id)) === JSON.stringify(requiredCases), 'fixture coverage changed');
assert(coverage.setupRule.includes('never insert Portal domain projections'), 'fixture setup must forbid projection writes');
for (const c of coverage.cases) {
  assert(fs.existsSync(path.join(workspace,c.source)), `${c.id}: fixture source missing: ${c.source}`);
  assert(c.step >= 2 && c.step <= 15 && c.gate, `${c.id}: missing executable gate owner`);
}

console.log(`PASS proposed runtime v1: ${contract.tools.length} native tools, ${contract.serviceOperations.length} restricted service operations, ${contract.internalContracts.length} internal contracts, ${keys.length} retirement keys, ${coverage.cases.length} fixture gates, positive and negative examples`);
