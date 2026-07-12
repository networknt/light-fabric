# light-workflow
An agentic workflow implemented in Rust

## Run a Local Test

Start the local light-portal stack first so Postgres, `workflow-command`, and
`workflow-query` are available:

```bash
cd /home/steve/workspace/portal-config-loc
./scripts/deploy-local.sh pg rust
```

Build the workflow engine binary from the `light-fabric` workspace root:

```bash
cd /home/steve/workspace/light-fabric
cargo build -p light-workflow --locked
```

Run it from this app directory with the portal Postgres URL:

```bash
cd /home/steve/workspace/light-fabric/apps/light-workflow
DATABASE_URL=postgres://postgres:secret@localhost:5432/configserver ./run.sh --debug-binary
```

For a multi-line shell command, either keep the assignments attached to
`./run.sh` with line continuations:

```bash
DATABASE_URL=postgres://postgres:secret@localhost:5432/configserver \
LIGHT_WORKFLOW_HTTP_ADDR=0.0.0.0:8436 \
RUST_LOG=light_workflow=debug,info \
WORKFLOW_LOG_ANSI=false \
./run.sh --debug-binary
```

or export the variables before starting the script:

```bash
export DATABASE_URL=postgres://postgres:secret@localhost:5432/configserver
export LIGHT_WORKFLOW_HTTP_ADDR=0.0.0.0:8436
export RUST_LOG=light_workflow=debug,info
export WORKFLOW_LOG_ANSI=false
./run.sh --debug-binary
```

Plain assignments on separate lines are shell-local variables, not environment
variables, so `./run.sh` cannot read them unless they are exported.

For repeated local runs, create `light-workflow.env` in this directory:

```bash
DATABASE_URL=postgres://postgres:secret@localhost:5432/configserver
LIGHT_WORKFLOW_HTTP_ADDR=0.0.0.0:8436
RUST_LOG=light_workflow=debug,info
WORKFLOW_LOG_ANSI=false
```

Then start the debug or release binary:

```bash
./run.sh --debug-binary
./run.sh
```

The script also accepts `--binary PATH` and `--env-file PATH`. `DATABASE_URL`
is required; `LIGHT_WORKFLOW_DATABASE_URL` and `WORKFLOW_DATABASE_URL` are
accepted aliases.

After `light-workflow` is running, create a workflow definition in
light-portal using one of the YAML files under `examples/`, then start the
workflow from the UI. The engine listens to `outbox_message_t`, creates the
first active task in `task_info_t`, and executes supported task types:
`ask`, `assert`, `call`, `set`, and `switch`.

Useful database checks:

```bash
psql "postgresql://postgres:secret@localhost:5432/configserver" \
  -c "select wf_def_id, name, version from wf_definition_t order by update_ts desc limit 5;"

psql "postgresql://postgres:secret@localhost:5432/configserver" \
  -c "select wf_task_id, task_type, status_code, task_output from task_info_t order by started_ts desc limit 10;"

psql "postgresql://postgres:secret@localhost:5432/configserver" \
  -c "select process_id, wf_instance_id, status_code, context_data from process_info_t order by started_ts desc limit 5;"
```

## Example Workflows

The examples are based on
`/home/steve/workspace/workflow-specification/schema/workflow.yaml` and are
kept parseable by `workflow-core`.

- `examples/simple-set-assert.yaml`: no external dependency; verifies `set`,
  `export`, and `assert`.
- `examples/http-risk-decision.yaml`: calls a local mock risk service at
  `http://127.0.0.1:18080/risk/evaluate`, branches with `switch`, and finishes
  with a normalized decision.
- `examples/human-approval.yaml`: creates an `ask` approval task and is useful
  for testing the waiting-task/worklist path.
- `examples/run-shell-mock-v1.yaml`: schedules the operator-approved
  `print-message` template through the isolated runner. Its matching local
  policy and template are in `config/runner-execution.mock.yml`; runner
  execution remains disabled unless `LIGHT_WORKFLOW_RUNNER_ENABLED=true`.

The versioned workflow execution policy schema and its valid/invalid
conformance fixtures are published under
`crates/workflow-policy/schema/` and `crates/workflow-policy/fixtures/`.

## Artifact object store

Runner artifact acceptance is fail-closed. When a terminal result declares an
artifact, `light-workflow` requires an S3-compatible object store and verifies
the runner's staging object before accepting the attempt. Configure it with
the standard AWS credential/workload-identity variables plus:

```bash
WORKFLOW_ARTIFACT_S3_BUCKET=workflow-artifacts
WORKFLOW_ARTIFACT_PREFIX=light-workflow
WORKFLOW_ARTIFACT_RETENTION_DAYS=30
# Optional for MinIO or another S3-compatible service:
WORKFLOW_ARTIFACT_S3_ENDPOINT=https://minio.example.net
# Development only:
# WORKFLOW_ARTIFACT_S3_ALLOW_HTTP=true
```

The store uses tenant-scoped `staging/<host_id>/` paths for short-lived uploads
and `tenants/<host_id>/objects/sha256/` keys for durable bytes. Identical bytes
from different tenants therefore never share retention or deletion authority.
Configure a bucket lifecycle rule to
expire abandoned `staging/` objects; the database remains authoritative for
durable retention. Promotion streams and hashes the staged object, performs a
provider-side copy only after the metadata row commits, re-verifies the copied
destination, deletes the staging key, and then fences the metadata transition
to `BOUND/VERIFIED`. Existing content-addressed destinations are also
re-verified before reuse. A digest
mismatch is quarantined and prevents workflow result acceptance.

The retention reconciler respects legal holds, claims deletions with
`SKIP LOCKED`, verifies object absence, retries with bounded backoff, recovers
stale delete claims, and retains the database tombstone. Successful attempts
also persist trusted execution provenance and bind its digest/reference to the
verified artifact rows before approval creation or task transition.

For the HTTP example, run any local mock that accepts:

```json
{
  "applicantId": "APP-LOW-RISK",
  "loanAmount": 100000,
  "creditScore": 820
}
```

and returns:

```json
{
  "riskScore": 15,
  "riskBand": "low"
}
```
