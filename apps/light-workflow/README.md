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
DATABASE_URL=postgres://postgres:secret@localhost:5432/configserver \
LIGHT_PORTAL_AUTHORIZATION="Bearer <workflow-service-token>" \
./run.sh --debug-binary
```

For a multi-line shell command, either keep the assignments attached to
`./run.sh` with line continuations:

```bash
DATABASE_URL=postgres://postgres:secret@localhost:5432/configserver \
LIGHT_PORTAL_AUTHORIZATION="Bearer <workflow-service-token>" \
SERVER_ENVIRONMENT=dev \
WORKFLOW_INVOCATION_CALLER_SERVICE_IDS=com.networknt.portal.gateway-1.0.0 \
LIGHTAPI_ENVIRONMENT=dev \
LIGHT_WORKFLOW_HTTP_ADDR=0.0.0.0:8436 \
WORKFLOW_MAXIMUM_PARALLELISM=64 \
RUST_LOG=light_workflow=debug,info \
WORKFLOW_LOG_ANSI=false \
./run.sh --debug-binary
```

or export the variables before starting the script:

```bash
export DATABASE_URL=postgres://postgres:secret@localhost:5432/configserver
export LIGHT_PORTAL_AUTHORIZATION="Bearer <workflow-service-token>"
export SERVER_ENVIRONMENT=dev
export WORKFLOW_INVOCATION_CALLER_SERVICE_IDS=com.networknt.portal.gateway-1.0.0
export LIGHTAPI_ENVIRONMENT=dev
export LIGHT_WORKFLOW_HTTP_ADDR=0.0.0.0:8436
export WORKFLOW_MAXIMUM_PARALLELISM=64
export RUST_LOG=light_workflow=debug,info
export WORKFLOW_LOG_ANSI=false
./run.sh --debug-binary
```

Plain assignments on separate lines are shell-local variables, not environment
variables, so `./run.sh` cannot read them unless they are exported.

For repeated local runs, create `light-workflow.env` in this directory:

```bash
DATABASE_URL=postgres://postgres:secret@localhost:5432/configserver
LIGHT_PORTAL_AUTHORIZATION="Bearer <workflow-service-token>"
SERVER_ENVIRONMENT=dev
WORKFLOW_INVOCATION_CALLER_SERVICE_IDS=com.networknt.portal.gateway-1.0.0
LIGHTAPI_ENVIRONMENT=dev
LIGHT_WORKFLOW_HTTP_ADDR=0.0.0.0:8436
WORKFLOW_MAXIMUM_PARALLELISM=64
RUST_LOG=light_workflow=debug,info
WORKFLOW_LOG_ANSI=false
```

Then start the debug or release binary:

```bash
./run.sh --debug-binary
./run.sh
```

The script also accepts `--binary PATH` and `--env-file PATH`. `DATABASE_URL`
and the workflow service's own `LIGHT_PORTAL_AUTHORIZATION` are required;
`LIGHT_WORKFLOW_DATABASE_URL` and `WORKFLOW_DATABASE_URL` are accepted database
aliases.

## Gateway/workflow authentication upgrade

The gateway and workflow service must be deployed together for the two-header
invocation contract: the initiating user's JWT is sent in `Authorization`, and
the gateway's own `LIGHT_PORTAL_AUTHORIZATION` is sent in `X-Scope-Token`.
Each service has exactly one service token under that generic environment name.
Mixed old/new gateway and workflow versions are intentionally rejected.

Before applying `patch_20260817_02_workflow_user_authorization.sql`, drain or
cancel every non-terminal row in `workflow_invocation_t`. The original user JWT
does not exist in pre-upgrade rows and cannot be backfilled safely, so the
migration fails closed when it finds one. Deploy the database patch, gateway,
and workflow images in the same maintenance window. New invocation credentials
are cleared when their invocation becomes terminal.

`SERVER_ENVIRONMENT` validates the gateway service token's `env` claim.
The workflow service's own `LIGHT_PORTAL_AUTHORIZATION` JWT must carry the same
`env` claim; startup rejects a missing or mismatched claim before that token can
be forwarded to a protected API in `X-Scope-Token`.
`LIGHTAPI_ENVIRONMENT` independently selects the workflow Tool grant and
LightAPI environment. Keep them explicit even when they currently have the
same value. `WORKFLOW_INVOCATION_CALLER_SERVICE_IDS` is a required comma-separated
allowlist of service IDs accepted in the gateway token's `sid` claim.

Workflow invocation JWT verification loads its JWKS during startup and fails
startup if the OAuth URL, CA certificate, or key response is unavailable.
The embedded defaults use `https://light-oauth:6881` and `/config/ca.pem`.
Override them with `CLIENT_TOKENKEYSERVERURL`, `CLIENT_CACERTPATH`, and
`CLIENT_VERIFYHOSTNAME`; native and Kubernetes deployments must mount the
configured CA path.

For a long-running asynchronous invocation, an authenticated status, wait, or
cancellation request refreshes the stored user JWT after checking
that its subject and disclosure claims still match. There is no safe automatic
refresh when no user or gateway lifecycle request occurs; callers must resume a
parked invocation with a current user JWT before its next protected HTTP task.

`WORKFLOW_MAXIMUM_PARALLELISM` is the service-wide hard ceiling for the number
of branches in a workflow fork. It defaults to 64 and must be between 1 and 64.
Both REST and event-driven workflow starts enforce this ceiling. A gateway or
Tool binding's `maximumParallelism` remains accepted on the wire for backward
compatibility, but it has no effect and is not enforced by `light-workflow`.

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

## Trusted fixed-action providers

`apply-patch` runs locally in a fresh hook/filter/submodule-disabled trusted
checkout. Branch/PR and publish/sign operations are sent as typed requests to
dedicated credential-owning services; no platform or signing credential is
placed in workflow context, a runner, an agent, or a sandbox. Configure one or
both providers:

```bash
WORKFLOW_REPOSITORY_FIXED_ACTION_URL=https://repository-actions.example.net/v1/
WORKFLOW_REPOSITORY_FIXED_ACTION_TOKEN=<service-to-service-token>
WORKFLOW_RELEASE_FIXED_ACTION_URL=https://release-actions.example.net/v1/
WORKFLOW_RELEASE_FIXED_ACTION_TOKEN=<service-to-service-token>
```

The repository service receives only `create-branch` and `open-pr`; the
release service receives only `publish` and `sign`. Requests contain the
consumed approval, immutable artifact and provenance digests, exact target,
policy digest, typed specification, and durable idempotency key. Providers
must exchange the service identity for an operation-scoped platform credential
and return a non-secret JSON receipt no larger than 64 KiB. Redirects are
disabled and provider URLs must use HTTPS. Unconfigured actions fail closed.
The receipt schema is closed and contains only `providerOperationId`,
`state: "SUCCEEDED"`, a SHA-256 `evidenceDigest`, and an optional bounded
`resourceReference`; additional fields are rejected rather than persisted.

`apps/light-github-action-provider` is the concrete repository provider. It
uses a fresh hardened Git checkout plus GitHub's refs and pull-request REST
APIs, an explicit repository allowlist and branch prefix, owner-only
service/GitHub token files, and a synchronous SQLite intent journal. The
workflow sends the canonical patch only after re-hashing it against the bound
verified artifact. The provider reapplies it to the exact base commit, creates
a deterministic commit, compare-and-set creates the branch, and opens a PR
only after the branch resolves to that commit. Lost create responses are
reconciled by inspecting the exact branch commit or PR head/base; replaying the
workflow idempotency key returns the stored receipt without overwriting an
existing branch or issuing another PR mutation.

Providers must also expose `GET fixed-actions/status` and resolve the same
`Idempotency-Key` to `SUCCEEDED`, `FAILED`, `PENDING`, or `NOT_FOUND` evidence.
After dispatch begins, exhausted 5xx/transport retries, a lost response,
malformed success evidence, or a service crash is recorded as `UNKNOWN`, never
as an ordinary failure. A leased reconciler inspects the provider without
reissuing the effect. `SUCCEEDED`/`FAILED` evidence closes the original
execution attempt; pending or unavailable evidence backs off for up to 24
hours. After that, the attempt becomes terminal `UNKNOWN`, automatic retry
remains prohibited, and the workflow waits with `FIXED_ACTION_UNKNOWN` for an
operator. A stale local `apply-patch` has no external status authority and
therefore becomes operator-required `UNKNOWN` once its execution deadline
expires.

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
