# Light Workflow Examples

This directory contains workflow definitions that can be imported into the
portal workflow definition table and started from the portal UI or command API.

## Prerequisites

- `light-workflow` is running against the same database as `workflow-command`.
- The workflow YAML has been created as a workflow definition in the portal.
- For `personalized-offer-rest-v1.yaml`, these Compose services are running:
  `demo-customer-profile-api` and `demo-offer-decision-api`.
- For `insurance-claim-rest-v1.yaml`, the same two demo API services are
  running, the agent catalog events have been imported, and the phase 2 demo
  API OpenAPI specs have been uploaded so the portal/tool catalog contains the
  insurance endpoints.
- For `insurance-claim-headless-v1.yaml`, the same REST services and agent
  catalog are available. This workflow does not create human task assignments.
- For `insurance-claim-mcp-v1.yaml`, `demo-customer-profile-api`,
  `demo-offer-decision-api`, `demo-insurance-claim-mcp-server`, and
  `light-gateway` are running. `tools/list` exposes `evaluateCoverage`,
  `classifyLiability`, `scoreClaimRisk`, `listRequiredDocuments`, and
  `generateCustomerSummary`.
- For `personalized-offer-mcp-v1.yaml`, `light-gateway` is running and the MCP
  tools are visible in the control plane:
  `getCustomerProfile`, `getCustomerPreferences`, `searchOffers`, and
  `recordOfferDecision`.
- For `agent-json-output.yaml`, import the demo agent catalog events with
  `event-importer`. The file uses the standard local host/user ids from
  `event-importer/events/local`; pass replacement rules when importing into
  another host:

```bash
cd /home/steve/workspace/event-importer
./importer.sh \
  --filename /home/steve/workspace/light-fabric/apps/light-workflow/examples/agent-catalog-events.json
```

For a different host/user:

```bash
./importer.sh \
  --filename /home/steve/workspace/light-fabric/apps/light-workflow/examples/agent-catalog-events.json \
  --replacement '[
    {"field":"hostId","from":"01964b05-552a-7c4b-9184-6857e7f3dc5f","to":"<host-id>"},
    {"field":"user","from":"01964b05-5532-7c79-8cde-191dcbd421b8","to":"<user-id>"},
    {"field":"operationOwner","from":"01964b05-5532-7c79-8cde-191dcbd421b8","to":"<user-id>"},
    {"field":"deliveryOwner","from":"01964b05-5532-7c79-8cde-191dcbd421b8","to":"<user-id>"}
  ]'
```

The event seed uses `modelProvider: mock` for deterministic local execution.
For a real model, submit update events through the portal command path or adjust
the event data before import. In phase 1, `apiKeyRef` is resolved as an
environment variable name such as `OPENAI_API_KEY`, or as `env:OPENAI_API_KEY`.

For `run-shell-mock-v1.yaml`, enable runner execution and use the checked-in
mock profile/template configuration:

```bash
export LIGHT_WORKFLOW_RUNNER_ENABLED=true
export LIGHT_WORKFLOW_RUNNER_CONFIG_FILE=/home/steve/workspace/light-fabric/apps/light-workflow/config/runner-execution.mock.yml
```

The matching runner must advertise `sha256:mock-ephemeral-v1`, and its
`allowedCommandTemplateDigests` must contain the canonical digest emitted for
the `print-message` template. Use `light-workflow-runner print-admission` after
placing that digest in the runner config; do not copy placeholder admission
digests into a deployment. The example has no network, credentials, persistent
workspace, or artifact export.

The `startWorkflow` command must send `input` as a JSON object, not as a JSON
string.

## Find the Workflow Id

After importing a definition, get the `hostId` and `wfDefId` from the UI or from
the local database:

```bash
psql "postgresql://postgres:secret@localhost:5432/configserver" \
  -c "select host_id, wf_def_id, namespace, name, version from wf_definition_t where active and name = 'personalized-offer-rest-v1';"
```

Replace the workflow name in the query for the other examples.

## Start a Workflow

From the UI, open the workflow definition and use the start action. Paste one of
the JSON input examples below.

From curl or Postman, call the portal command endpoint. Replace `<host-id>`,
`<workflow-definition-id>`, and `<access-token>`.

```bash
curl -k -X POST "https://localhost:8443/portal/command" \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer <access-token>" \
  -d '{
    "host": "lightapi.net",
    "service": "workflow",
    "action": "startWorkflow",
    "version": "0.1.0",
    "data": {
      "hostId": "<host-id>",
      "wfDefId": "<workflow-definition-id>",
      "input": {
        "customerId": "CUST-1001",
        "channel": "portal"
      }
    }
  }'
```

If the local UI uses a browser session instead of a bearer token, copy the
authorization header from the browser network request or use the UI start form.

## Verify Execution

```bash
psql "postgresql://postgres:secret@localhost:5432/configserver" \
  -c "select wf_instance_id, status_code, input_data, context_data from process_info_t order by started_ts desc limit 5;"

psql "postgresql://postgres:secret@localhost:5432/configserver" \
  -c "select wf_instance_id, wf_task_id, task_type, status_code, task_output from task_info_t order by started_ts desc limit 10;"

psql "postgresql://postgres:secret@localhost:5432/configserver" \
  -c "select task_id, assignment_type, assignment_id, category_code, status_code, active from task_asst_t order by assigned_ts desc limit 10;"
```

Status codes:

| Code | Meaning |
| ---- | ------- |
| `A` | Active task or process. |
| `C` | Completed task or process. |
| `W` | Task is waiting for human input. |
| `F` | Failed task or process. |

## Insurance Claim Demo Runbook

Use this section for the full agentic insurance-claim demo. It packages the
REST, MCP, and headless workflow variants with repeatable command and SQL
helpers.

### Setup Checklist

1. Start the local portal stack with Postgres, `workflow-command`,
   `workflow-query`, and `light-gateway`.
2. Start `light-workflow` with the same `DATABASE_URL` as the portal stack.
3. Start `demo-customer-profile-api`, `demo-offer-decision-api`, and
   `demo-insurance-claim-mcp-server`.
4. Upload or refresh the phase 2 OpenAPI specs for both demo APIs.
5. Import `agent-catalog-events.json` through `event-importer`.
6. Verify the roles `claimant`, `claims-adjuster`, `siu-investigator`, and
   `customer-service` exist in the host used for the demo.
7. Create workflow definitions for `insurance-claim-rest-v1.yaml`,
   `insurance-claim-mcp-v1.yaml`, and `insurance-claim-headless-v1.yaml`.
8. Confirm `tools/list` exposes the five insurance claim MCP gap tools before
   running the MCP workflow.

Use this query to capture the workflow ids needed by curl or Postman:

```bash
psql "postgresql://postgres:secret@localhost:5432/configserver" \
  -c "select host_id, wf_def_id, name from wf_definition_t where active and name in ('insurance-claim-rest-v1', 'insurance-claim-mcp-v1', 'insurance-claim-headless-v1') order by name;"
```

Use this JSON-RPC call to verify the MCP tool surface:

```bash
curl -k -sS -X POST "https://localhost:8443/mcp" \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer <access-token>" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}'
```

### Curl And Postman

The curl helper starts each workflow and completes the two common human-task
pauses:

```bash
cd /home/steve/workspace/light-fabric/apps/light-workflow/examples

ACCESS_TOKEN=<token> \
HOST_ID=<host-id> \
HEADLESS_WF_DEF_ID=<headless-wf-def-id> \
./insurance-claim-demo-curl.sh start-headless

ACCESS_TOKEN=<token> \
HOST_ID=<host-id> \
REST_WF_DEF_ID=<rest-wf-def-id> \
./insurance-claim-demo-curl.sh start-rest
```

After the REST or MCP workflow creates a human task, use the SQL helper below to
copy `task_id` and `task_asst_id`, then run:

```bash
ACCESS_TOKEN=<token> \
HOST_ID=<host-id> \
TASK_ASST_ID=<task-asst-id> \
./insurance-claim-demo-curl.sh claim-task

ACCESS_TOKEN=<token> \
HOST_ID=<host-id> \
TASK_ID=<task-id> \
TASK_ASST_ID=<task-asst-id> \
./insurance-claim-demo-curl.sh complete-adjuster-approved

ACCESS_TOKEN=<token> \
HOST_ID=<host-id> \
TASK_ID=<next-task-id> \
TASK_ASST_ID=<next-task-asst-id> \
./insurance-claim-demo-curl.sh complete-claimant-accepted
```

Import `insurance-claim-demo.postman_collection.json` into Postman for the same
requests. Fill collection variables for `portalCommandUrl`, `accessToken`,
`hostId`, and the three workflow definition ids.

### SQL Verification And Reset

Run the verification helper after each start or task completion:

```bash
psql "postgresql://postgres:secret@localhost:5432/configserver" \
  -v host_id=<host-id> \
  -f /home/steve/workspace/light-fabric/apps/light-workflow/examples/insurance-claim-demo-queries.sql
```

Preview active reset candidates:

```bash
psql "postgresql://postgres:secret@localhost:5432/configserver" \
  -v host_id=<host-id> \
  -f /home/steve/workspace/light-fabric/apps/light-workflow/examples/insurance-claim-demo-reset.sql
```

Reset active insurance claim demo instances through `ProcessInfoDeletedEvent`:

```bash
cd /home/steve/workspace/light-fabric/apps/light-workflow/examples

ACCESS_TOKEN=<token> \
HOST_ID=<host-id> \
./insurance-claim-demo-reset.sh
```

The reset uses `workflow/deleteProcessInfo`; the projection soft-deletes
`process_info_t.active`. It keeps workflow definitions, event-store rows,
imported agent catalog data, and uploaded API metadata intact.

### Troubleshooting

| Symptom | Check |
| ------- | ----- |
| `startWorkflow` succeeds but fields resolve as `${ .customerId }` | The command sent `input` as a string. Send it as a JSON object. |
| `loadCustomerProfile` fails | Verify `demo-customer-profile-api` is reachable from `light-workflow`; Docker runs should use the Compose service name. |
| `triageClaim` or `recommendSettlement` fails | Verify `demo-offer-decision-api` is running and the phase 2 endpoints are present. |
| Agent task fails before a human task | Confirm `agent-catalog-events.json` was imported for the same `hostId`; inspect `_agentAudit` with `insurance-claim-demo-queries.sql`. |
| MCP task fails with tool not found | Call `tools/list` on `light-gateway` and confirm the five insurance claim MCP tool names match the workflow YAML exactly. |
| Human task is not visible | Confirm the role assignment row in `task_asst_t`, the user's role membership, and that the task status is `W` or assignment status is `ASSIGNED`. |
| Headless workflow creates an assignment | You started the REST or MCP definition instead of `insurance-claim-headless-v1`. |

## Workflow Inputs

### `simple-set-assert.yaml`

Local smoke test for `set`, `export`, `assert`, and sequential transitions. It
has no external service dependency.

```json
{
  "applicantId": "APP-001"
}
```

Expected behavior:

| Input | Expected result |
| ----- | --------------- |
| Any `applicantId` string | Completes with `status: APPROVED`, `verified: true`, and message `Workflow completed by simple-set-assert`. |

### `human-approval.yaml`

Creates an assigned human approval task.

```json
{
  "requestId": "REQ-001",
  "summary": "Approve test request"
}
```

Expected behavior:

| Input | Expected result |
| ----- | --------------- |
| Valid `requestId` and `summary` | Stops at `requestApproval` with task status `W`; creates a `task_asst_t` row for role `admin`, category `approval`, reason `human-approval`. |
| Approval completed with `APPROVED` or `REJECTED` | Resumes and records the selected approval payload in `recordDecision`. |

### `agent-json-output.yaml`

Runs a native `call: agent` task, validates the returned JSON against an inline
workflow schema reference, exports it into workflow context, and persists a
compact `_agentAudit` field in task output.

```json
{
  "claimId": "CLM-1001",
  "accidentDescription": "Rear-end collision at a traffic light. No injuries reported."
}
```

Expected behavior:

| Input | Expected result |
| ----- | --------------- |
| Valid `claimId` and `accidentDescription` | Completes with `intakeSummary.summary`, `missingInformation`, `requiresHumanReview`, and `_agentAudit`. |
| Invalid agent JSON after retry exhaustion | Routes to `requestManualReview` and creates a `claims-adjuster` role assignment. |

### `http-risk-decision.yaml`

Calls a mock HTTP risk service at
`http://127.0.0.1:18080/risk/evaluate`, then branches on the returned
`riskScore`.

Input:

```json
{
  "applicantId": "APP-LOW-RISK",
  "loanAmount": 100000,
  "creditScore": 820
}
```

The mock service should accept the workflow input fields and return JSON like:

```json
{
  "riskScore": 15,
  "riskBand": "low"
}
```

Expected behavior depends on the mock response:

| Mock `riskScore` | Expected result |
| ---------------- | --------------- |
| `<= 30` | Completes with `status: APPROVED`. |
| `31` to `79` | Completes with `status: REVIEW_REQUIRED`. |
| `>= 80` | Completes with `status: REJECTED`. |
| HTTP non-2xx or unreachable mock | The `evaluateRisk` task fails and the process status becomes `F`. |

When `light-workflow` runs in Docker, `127.0.0.1` means the workflow container,
not the host. Use a Compose service name or `host.docker.internal` if the mock
service runs outside the container.

### `personalized-offer-rest-v1.yaml`

Calls the demo APIs directly:

- `demo-customer-profile-api`
- `demo-offer-decision-api`

Base input:

```json
{
  "customerId": "CUST-1001",
  "channel": "portal"
}
```

Seeded customer behavior:

| `customerId` | Expected result |
| ------------ | --------------- |
| `CUST-1001` | Loads a premium customer with consent, finds `OFFER-TRAVEL-01`, and stops at `requestApproval` with status `W`. It creates an `admin` role assignment in category `approval` with reason `personalized-offer-rest`. |
| `CUST-2002` | Loads a standard customer with consent, does not enter the premium offer path, and completes with `status: NO_ELIGIBLE_OFFER`. |
| `CUST-3003` | Loads a premium customer without consent and completes with `status: NO_CONSENT`. |
| Unknown customer, for example `CUST-0001` | The profile API returns 404, the `loadProfile` task fails, and the process status becomes `F`. |

Approval behavior for `CUST-1001`:

| Human decision | Expected result |
| -------------- | --------------- |
| `APPROVED` | Records the offer decision through `demo-offer-decision-api` and completes with `status: APPROVED`, `selectedOfferId`, and `decisionId`. |
| `REJECTED` | Skips the decision API and completes with `status: REJECTED`. |

### `insurance-claim-rest-v1.yaml`

Runs the phase 3 insurance claim demo over direct REST calls. It loads customer,
policy, vehicle, and prior-claim data, runs the three bounded agents, creates
durable human tasks, and posts the final settlement recommendation.

Input:

```json
{
  "customerId": "CUST-1001",
  "vehicleId": "VEH-1001",
  "incidentDate": "2026-05-30",
  "accidentDescription": "Rear-ended at an intersection. No injuries reported.",
  "location": "Ottawa, ON",
  "injuryReported": false,
  "vehicleDrivable": false,
  "policeReportFiled": true,
  "photosAvailable": true,
  "channel": "portal"
}
```

Seeded customer behavior:

| Input | Expected result |
| ----- | --------------- |
| `CUST-1001` with `VEH-1001` | Loads an active policy and covered vehicle, runs intake and coverage agents, posts claim triage, and stops at `requestAdjusterApproval`. It creates a `claims-adjuster` assignment in category `approval` with reason `claim-review`. |
| Intake agent returns `requiresHumanInput: true` | Stops at `askMissingInfo`. It creates a `customer-service` assignment in category `claim-info` with reason `missing-fnol-details`. |
| Adjuster completes with `APPROVED` | Runs the settlement agent, posts the settlement recommendation, and stops at `requestCustomerResponse`. It creates a `claimant` assignment in category `claim-response` with reason `settlement-response`. |
| Claimant completes with `ACCEPT_REPAIR` | Completes with `status: CLAIM_APPROVED`, `claimId`, `customerId`, `decisionId`, and `customerSummary`. |
| `CUST-2002` with `VEH-2002` | Loads an expired policy and uncovered vehicle. The coverage and triage path can create a `claims-adjuster` or `siu-investigator` assignment depending on the review output. |
| `CUST-3003` with `VEH-3003` | Loads a customer without communication consent. The claim can still be reviewed, but the workflow keeps the explicit human approval path for settlement. |
| Unknown customer or vehicle | The failing API call returns 404, the current task fails, and the process status becomes `F`. |

Human task completion values:

| Task | Values |
| ---- | ------ |
| `requestAdjusterApproval` | `APPROVED`, `REJECTED`, `REQUEST_MORE_INFO`, `REFER_TO_SIU` |
| `requestSiuReview` | `CLEAR`, `REFER_BACK_TO_ADJUSTER`, `HOLD_FOR_INVESTIGATION` |
| `requestCustomerResponse` | `ACCEPT_REPAIR`, `REQUEST_CALLBACK`, `UPLOAD_MORE_DOCUMENTS`, `DISPUTE_RECOMMENDATION` |

### `insurance-claim-mcp-v1.yaml`

Runs the same claim workflow as `insurance-claim-rest-v1.yaml`, but deliberately
mixes REST APIs and MCP tools. Existing customer, triage, and settlement
capabilities stay on the REST demo APIs. MCP is used only for the coverage,
liability, risk, document, and customer-summary gaps exposed by
`demo-insurance-claim-mcp-server` through `light-gateway`.

Use the same input as `insurance-claim-rest-v1.yaml`.

Expected MCP tools:

| Tool | Context export |
| ---- | -------------- |
| `evaluateCoverage` | `coverage` from `structuredContent` |
| `classifyLiability` | `liability` from `structuredContent` |
| `scoreClaimRisk` | `risk` from `structuredContent` |
| `listRequiredDocuments` | `requiredDocuments` from `structuredContent` |
| `generateCustomerSummary` | `customerSummary` from `structuredContent` |

Expected behavior matches the REST workflow for the same seeded inputs.
`assertMcpTriage` and `assertMcpSettlement` fail fast if a REST or MCP call
returns malformed output or omits required fields. If a tool is missing, the
current MCP task fails and the process status becomes `F`.

### `insurance-claim-headless-v1.yaml`

Runs the same REST-backed insurance claim path without `ask` tasks. Use it for
CI, scheduled smoke tests, and repeatable local regression checks.
The same cases are captured in `insurance-claim-headless-regression-cases.json`
for script-driven runs.

Happy path input:

```json
{
  "customerId": "CUST-1001",
  "vehicleId": "VEH-1001",
  "incidentDate": "2026-05-30",
  "accidentDescription": "Rear-ended at an intersection. No injuries reported.",
  "location": "Ottawa, ON",
  "injuryReported": false,
  "vehicleDrivable": false,
  "policeReportFiled": true,
  "photosAvailable": true,
  "channel": "portal",
  "simulateMissingInfo": false,
  "missingFields": [],
  "approvalMode": "auto-approve",
  "siuMode": "clear",
  "customerResponseMode": "accept-repair"
}
```

Control fields:

| Field | Values |
| ----- | ------ |
| `approvalMode` | `auto-approve`, `auto-reject`, `request-more-info`, `refer-siu` |
| `siuMode` | `clear`, `hold` |
| `customerResponseMode` | `accept-repair`, `callback`, `upload-more-documents`, `dispute` |

Regression cases:

| Case | Input change | Expected result |
| ---- | ------------ | --------------- |
| Happy path | Use the sample input. | Completes with `status: CLAIM_APPROVED` and no `task_asst_t` assignment rows. |
| Missing info | Set `simulateMissingInfo: true` and `missingFields: ["vehicleDrivable"]`. | Completes with `status: NEEDS_CUSTOMER_INFO` at `headlessMissingInfoResult`. |
| SIU referral | Use `CUST-2002` with `VEH-2002` and set `siuMode: hold`. | Completes with `status: REFERRED_TO_SIU` at `siuHoldResult`. |
| Unknown customer | Use `CUST-0001` with `VEH-0001`. | The profile API returns 404, the current task fails, and the process status becomes `F`. |

### `personalized-offer-mcp-v1.yaml`

Runs the same business flow as the REST workflow, but invokes the demo APIs
through MCP tools exposed by `light-gateway`.

Use the same input cases as `personalized-offer-rest-v1.yaml`:

```json
{
  "customerId": "CUST-1001",
  "channel": "portal"
}
```

Expected behavior:

| `customerId` | Expected result |
| ------------ | --------------- |
| `CUST-1001` | Calls MCP tools, finds the travel offer, and stops at `requestApproval` with an `admin` role assignment in category `approval` and reason `personalized-offer-mcp`. |
| `CUST-2002` | Completes with `status: NO_ELIGIBLE_OFFER`. |
| `CUST-3003` | Completes with `status: NO_CONSENT`. |
| Unknown customer | The profile tool call fails and the process status becomes `F`. |

Approval behavior matches the REST workflow: `APPROVED` records the offer
decision, and `REJECTED` completes without recording it.
