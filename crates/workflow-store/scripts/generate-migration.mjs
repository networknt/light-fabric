#!/usr/bin/env node

import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const workspaceDir = resolve(scriptDir, "../../..");
const sourcePath = process.argv[2] || resolve(workspaceDir, "../portal-db/postgres/ddl.sql");
const outputPath = process.argv[3] || resolve(
  scriptDir,
  "../migrations/workflow-postgres/0001_workflow_runtime.sql",
);

export const authorityTables = [
  "process_info_t",
  "task_info_t",
  "workflow_approval_t",
  "workflow_artifact_t",
  "workflow_executor_tenant_turn_t",
  "workflow_fork_branch_t",
  "workflow_fork_join_t",
  "workflow_invocation_audit_outbox_t",
  "workflow_invocation_budget_reservation_t",
  "workflow_invocation_budget_t",
  "workflow_invocation_event_quarantine_t",
  "workflow_invocation_idempotency_t",
  "workflow_invocation_t",
  "workflow_task_effect_t",
  "workflow_tool_access_request_item_t",
  "workflow_tool_access_request_t",
  "workflow_tool_approval_evidence_t",
];

export const projectionTables = [
  "wf_definition_t",
  "workflow_endpoint_target_t",
  "workflow_execution_policy_t",
  "workflow_tool_binding_t",
  "workflow_tool_dependency_t",
  "workflow_tool_grant_t",
];

const functions = [
  "workflow_claim_host_task_v1",
  "workflow_claim_idempotency_v1",
  "workflow_claim_task_effect_v1",
  "workflow_confirm_task_effect_v1",
  "workflow_reconcile_budget_v1",
  "workflow_reserve_budget_v1",
];

const source = readFileSync(sourcePath, "utf8");
const tables = [...authorityTables, ...projectionTables];
const selected = new Set(tables);

function requiredMatch(expression, label) {
  const match = source.match(expression);
  if (!match) throw new Error(`canonical Portal DDL is missing ${label}`);
  return match[0];
}

const blocks = [];
for (const table of tables) {
  blocks.push(requiredMatch(
    new RegExp(`CREATE TABLE public\\.${table} \\([\\s\\S]*?\\n\\);`),
    `table ${table}`,
  ));
}

for (const match of source.matchAll(/ALTER TABLE ONLY public\.([a-z0-9_]+)\s+ADD CONSTRAINT[\s\S]*?;\n/g)) {
  const table = match[1];
  if (!selected.has(table)) continue;
  const references = [...match[0].matchAll(/REFERENCES public\.([a-z0-9_]+)/g)].map(item => item[1]);
  if (references.some(target => !selected.has(target))) continue;
  blocks.push(match[0].trim());
}

for (const match of source.matchAll(/CREATE (?:UNIQUE )?INDEX [a-z0-9_]+ ON public\.([a-z0-9_]+)[\s\S]*?;\n/g)) {
  if (selected.has(match[1])) blocks.push(match[0].trim());
}

for (const name of functions) {
  blocks.push(requiredMatch(
    new RegExp(`CREATE FUNCTION public\\.${name}\\([\\s\\S]*?\\n\\$\\$;`),
    `function ${name}`,
  ));
}

const normalized = blocks
  .join("\n\n")
  .replaceAll("public.", "workflow_ops.");

const header = `-- Generated deterministically from portal-db/postgres/ddl.sql.
-- Workflow operational authority is reset/reseeded in early development; this
-- migration deliberately contains no source-row copy or dual-write machinery.

SET search_path TO workflow_ops, pg_catalog;

`;

const footer = `

GRANT USAGE ON SCHEMA workflow_ops TO operations_workflow_runtime;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA workflow_ops TO operations_workflow_runtime;
GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA workflow_ops TO operations_workflow_runtime;
ALTER DEFAULT PRIVILEGES FOR ROLE operations_workflow_migrator IN SCHEMA workflow_ops
  GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO operations_workflow_runtime;
ALTER DEFAULT PRIVILEGES FOR ROLE operations_workflow_migrator IN SCHEMA workflow_ops
  GRANT EXECUTE ON FUNCTIONS TO operations_workflow_runtime;
`;

mkdirSync(dirname(outputPath), { recursive: true });
writeFileSync(outputPath, `${header}${normalized}${footer}`);
