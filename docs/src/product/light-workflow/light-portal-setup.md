# Light Portal Setup

This page describes the portal-side setup required to run the
`light-workflow` product demos from a local `light-portal` stack.

For the execution model behind native agent tasks, see
[Native Agent Call](native-agent-call.md). For the insurance product scenario,
see [Insurance Claim Agentic Workflow](insurance-claim-agentic-workflow.md).

## Prerequisites

Start the local portal stack with the workflow services, gateway, controller,
and Postgres available.

For the Rust local stack:

```bash
cd /home/steve/workspace/portal-config-loc
./scripts/deploy-local.sh pg rust
```

The local stack should include:

- Postgres,
- `workflow-command`,
- `workflow-query`,
- `light-gateway`,
- controller,
- config-server,
- `demo-customer-profile-api`,
- `demo-offer-decision-api`.

`light-workflow` must use the same database as `workflow-command`:

```bash
DATABASE_URL=postgres://postgres:secret@localhost:5432/configserver
```

## Start Light-Workflow

Build and run `light-workflow` from the `light-fabric` checkout:

```bash
cd /home/steve/workspace/light-fabric
cargo build -p light-workflow --locked

cd apps/light-workflow
DATABASE_URL=postgres://postgres:secret@localhost:5432/configserver \
LIGHT_WORKFLOW_HTTP_ADDR=0.0.0.0:8080 \
RUST_LOG=light_workflow=debug,info \
WORKFLOW_LOG_ANSI=false \
./run.sh --debug-binary
```

For repeated runs, put those values in
`apps/light-workflow/light-workflow.env` and run:

```bash
./run.sh --debug-binary
```

## Import Agent Catalog Data

Native `call: agent` tasks load portal agent, skill, and tool metadata from the
portal database. Import the demo catalog events before running workflows that
contain agent tasks.

```bash
cd /home/steve/workspace/event-importer
./importer.sh \
  --filename /home/steve/workspace/light-fabric/apps/light-workflow/examples/agent-catalog-events.json
```

For a different host or user, pass replacement rules:

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

The demo catalog uses `modelProvider: mock` for deterministic local runs. For
real model execution, update the portal agent definitions to use the desired
provider and `apiKeyRef`.

## Upload API Metadata

For the insurance claim demos, upload or refresh the OpenAPI specs for:

- `demo-customer-profile-api`,
- `demo-offer-decision-api`.

The portal catalog should contain endpoint and tool projections for the demo
APIs before the MCP workflow is run. The MCP workflow expects `light-gateway`
`tools/list` to expose these tools:

```text
getCustomerProfile
getCustomerPreferences
getCustomerPolicies
getCoveredVehicle
listPriorClaims
triageClaim
recommendSettlement
```

Verify the tool surface through the gateway:

```bash
curl -k -sS -X POST "https://localhost:8443/mcp" \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer <access-token>" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}'
```

## Create Workflow Definitions

Create workflow definitions in the portal UI or through the workflow command
API. For the insurance claim demo, create these definitions:

```text
insurance-claim-rest-v1.yaml
insurance-claim-mcp-v1.yaml
insurance-claim-headless-v1.yaml
```

The files live in:

```text
/home/steve/workspace/light-fabric/apps/light-workflow/examples
```

After creation, capture their ids:

```bash
psql "postgresql://postgres:secret@localhost:5432/configserver" \
  -c "select host_id, wf_def_id, name from wf_definition_t where active and name in ('insurance-claim-rest-v1', 'insurance-claim-mcp-v1', 'insurance-claim-headless-v1') order by name;"
```

## Roles And Human Tasks

The insurance claim workflow creates durable human tasks. Confirm that the demo
host has the roles used by those assignments:

```text
claimant
claims-adjuster
siu-investigator
customer-service
```

Human tasks remain in the portal database while waiting. The workflow resumes
after the task-completion command records a valid response.

## Start And Verify

Use the portal UI start action, Postman collection, or curl helper from the
examples directory.

```bash
cd /home/steve/workspace/light-fabric/apps/light-workflow/examples

ACCESS_TOKEN=<token> \
HOST_ID=<host-id> \
HEADLESS_WF_DEF_ID=<headless-wf-def-id> \
./insurance-claim-demo-curl.sh start-headless
```

Run the SQL verification helper after each start or task completion:

```bash
psql "postgresql://postgres:secret@localhost:5432/configserver" \
  -v host_id=<host-id> \
  -f /home/steve/workspace/light-fabric/apps/light-workflow/examples/insurance-claim-demo-queries.sql
```

For the full runbook, see:

```text
/home/steve/workspace/light-fabric/apps/light-workflow/examples/README.md
```

## Troubleshooting

| Symptom | Check |
| --- | --- |
| Workflow starts but no process appears | Confirm `light-workflow` uses the same `DATABASE_URL` as `workflow-command`. |
| Agent task fails before a human task | Confirm `agent-catalog-events.json` was imported for the same `hostId`. |
| MCP tool is not found | Call gateway `tools/list` and confirm the tool names match the workflow YAML. |
| Human task is not visible | Check `task_asst_t`, role membership, and task status. |
| Input fields resolve as `${ .customerId }` | Confirm `startWorkflow` sends `input` as a JSON object, not a JSON string. |
