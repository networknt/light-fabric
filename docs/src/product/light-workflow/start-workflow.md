# Start Workflow

This page describes the local workflow start path used to test `light-workflow`
from `light-portal`.

`light-workflow` does not create workflow definitions and it is not the public
entry point for starting a workflow. For local testing, create the workflow
definition through the portal workflow service, then start it through the
`startWorkflow` command. The running `light-workflow` process consumes the
workflow start event from the portal database and executes the workflow tasks.

## Runtime Path

The local start flow is:

1. Create or update a workflow definition in `light-portal`.
2. Start the workflow with the `workflow` service `startWorkflow` command.
3. `workflow-command` writes a workflow started event into the event store and
   outbox tables.
4. `light-workflow` polls the same database, loads the definition by `wfDefId`,
   creates the process and task records, and executes the workflow.

For this reason, the `DATABASE_URL` used by `light-workflow` must point to the
same database used by the local portal stack.

## Prerequisites

Start the local portal stack first. For the Rust local stack, use the normal
portal-config-local deployment command from the `portal-config-loc` checkout:

```bash
./scripts/deploy-local.sh pg rust
```

Make sure the workflow command and query services are available in that stack.
The workflow definition pages in `portal-view` depend on those services.

Then build `light-workflow`:

```bash
cd /home/steve/workspace/light-fabric/apps/light-workflow
cargo build -p light-workflow --locked
```

## Start light-workflow Locally

Create `light-workflow.env` in
`/home/steve/workspace/light-fabric/apps/light-workflow`:

```bash
DATABASE_URL=postgres://postgres:secret@localhost:5432/configserver
LIGHT_WORKFLOW_HTTP_ADDR=0.0.0.0:8080
RUST_LOG=light_workflow=debug,info
WORKFLOW_LOG_ANSI=false
```

Start the service with the debug binary:

```bash
./run.sh --debug-binary
```

The script loads `light-workflow.env` automatically. If you do not use the env
file, export the values before running the script:

```bash
export DATABASE_URL=postgres://postgres:secret@localhost:5432/configserver
export LIGHT_WORKFLOW_HTTP_ADDR=0.0.0.0:8080
export RUST_LOG=light_workflow=debug,info
export WORKFLOW_LOG_ANSI=false
./run.sh --debug-binary
```

Do not set the variables on separate shell lines without `export`. That creates
shell variables only for the current shell and `run.sh` will not receive them.

## Recommended UI Test

The easiest local test is to create the definition in the portal UI and start it
from the workflow editor test action.

1. Open `light-portal`.
2. Go to the workflow definition page.
3. Create a workflow definition.
4. Paste one of the example workflow YAML files from:

   ```text
   /home/steve/workspace/light-fabric/apps/light-workflow/examples
   ```

5. Save the definition.
6. Open the definition in the workflow editor.
7. Use the editor test run action with a JSON input object.

For the basic example, use
`apps/light-workflow/examples/simple-set-assert.yaml` and this input:

```json
{
  "applicantId": "APP-001"
}
```

The editor test action is preferred for local testing because it parses the
input text as JSON and sends `input` as an object.

The table run button opens the generic `startWorkflow` form. If using that path,
make sure the request sends `input` as a JSON object, not as a string. If the
input is submitted as a string, the workflow command may accept the request but
the runtime context will not have the expected object fields.

## Start with Postman or curl

You can also start the workflow directly through the portal command endpoint.
Send the request to the same light-gateway or light-portal host used by the UI.
Do not send this request to `light-workflow`; `light-workflow` is the executor,
not the command API.

The command envelope is:

```json
{
  "host": "lightapi.net",
  "service": "workflow",
  "action": "startWorkflow",
  "version": "0.1.0",
  "data": {
    "hostId": "<host-id>",
    "wfDefId": "<workflow-definition-id>",
    "input": {
      "applicantId": "APP-001"
    }
  }
}
```

Example curl shape:

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
        "applicantId": "APP-001"
      }
    }
  }'
```

If your local UI uses a session cookie instead of a bearer token, use Postman
with the same authenticated session or copy the current local authorization
header from the browser request.

## Creating the Definition by API

For most local tests, create the definition in the UI. It is easier because the
YAML can be pasted directly.

If you create the definition through the command API, send a workflow definition
command first and use the returned definition id as `wfDefId` in the
`startWorkflow` command.

The command shape is:

```json
{
  "host": "lightapi.net",
  "service": "workflow",
  "action": "createWfDefinition",
  "version": "0.1.0",
  "data": {
    "hostId": "<host-id>",
    "namespace": "light-portal",
    "name": "simple-set-assert",
    "version": "1.0.0",
    "definition": "<workflow-yaml-as-json-string>"
  }
}
```

When calling this from Postman, remember that the YAML definition is a JSON
string field. Newlines must be escaped correctly by the JSON editor or sent by a
tool that can build the JSON body safely.

## Example Workflows

The current examples are in
`/home/steve/workspace/light-fabric/apps/light-workflow/examples`:

| File | Purpose | Input |
| ---- | ------- | ----- |
| `simple-set-assert.yaml` | Basic local smoke test with no external dependency. | `{ "applicantId": "APP-001" }` |
| `http-risk-decision.yaml` | Calls a risk evaluation HTTP endpoint and branches on the result. | `{ "applicantId": "APP-001", "loanAmount": 25000, "creditScore": 720 }` |
| `human-approval.yaml` | Creates a human approval style workflow and waits for a later decision. | `{ "requestId": "REQ-001", "summary": "Approve test request" }` |

Start with `simple-set-assert.yaml`. It is the best smoke test because it does
not require another service.

For `http-risk-decision.yaml`, start a local mock service for the URL used by
the definition. When `light-workflow` runs natively with `run.sh`,
`127.0.0.1` means the host machine. When `light-workflow` runs in Docker,
`127.0.0.1` means the container itself, so change the workflow endpoint to a
Compose service name or `host.docker.internal`.

For `human-approval.yaml`, the first run should create a waiting task. Completing
that flow requires the worklist or task-completion API path.

## Verify Execution

Watch the `light-workflow` log after sending `startWorkflow`. A successful run
should show that the start event was received, the first task was initialized,
and the executor picked up task work.

Useful database checks:

```sql
select wf_def_id, namespace, name, version
from wf_definition_t
order by update_ts desc
limit 5;

select process_id, wf_instance_id, status_code, context_data
from process_info_t
order by started_ts desc
limit 5;

select wf_task_id, task_type, status_code, task_output
from task_info_t
order by started_ts desc
limit 10;

select c_offset, event_type, aggregate_id, payload
from outbox_message_t
order by c_offset desc
limit 10;
```

If `outbox_message_t` has the workflow started event but no process or task
records appear, check that `light-workflow` is running against the same
`DATABASE_URL` as the portal stack.

## Troubleshooting

- `DATABASE_URL is required`: Put `DATABASE_URL` in `light-workflow.env`,
  export it before running `run.sh`, or put the assignment on the same command
  line as `./run.sh`.
- `function make_interval(mins => bigint) does not exist`: Rebuild and restart
  `light-workflow`. The runtime query must cast the retry value to `int` before
  passing it to `make_interval`.
- Workflow definition list is empty in the UI: Confirm the workflow query
  service is running and the local stack is using the jar or binary that
  contains the workflow definition owner-scope fix. Some local stacks run copied
  service artifacts, so rebuilding a source checkout is not enough unless the
  deployed artifact is refreshed.
- No tasks are created after starting the workflow: Confirm the `startWorkflow`
  command wrote a workflow started event to the outbox table, and confirm
  `light-workflow` points to that same database.
- The workflow input is missing fields: Confirm `input` was submitted as a JSON
  object. A string that contains JSON text is not the same as a JSON object in
  the workflow context.
