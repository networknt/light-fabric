# Light Workflow Examples

This directory contains workflow definitions that can be imported into the
portal workflow definition table and started from the portal UI or command API.

## Prerequisites

- `light-workflow` is running against the same database as `workflow-command`.
- The workflow YAML has been created as a workflow definition in the portal.
- For `personalized-offer-rest-v1.yaml`, these Compose services are running:
  `demo-customer-profile-api` and `demo-offer-decision-api`.
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
