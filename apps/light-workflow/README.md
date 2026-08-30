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
LIGHT_WORKFLOW_CONFIG_MODE=local \
SERVER_ENVIRONMENT=dev \
./run.sh --debug-binary
```

For a multi-line shell command, either keep the assignments attached to
`./run.sh` with line continuations:

```bash
DATABASE_URL=postgres://postgres:secret@localhost:5432/configserver \
LIGHT_PORTAL_AUTHORIZATION="Bearer <workflow-service-token>" \
SERVER_ENVIRONMENT=dev \
LIGHT_WORKFLOW_CONFIG_MODE=local \
RUST_LOG=light_workflow=debug,info \
WORKFLOW_LOG_ANSI=false \
./run.sh --debug-binary
```

or export the variables before starting the script:

```bash
export DATABASE_URL=postgres://postgres:secret@localhost:5432/configserver
export LIGHT_PORTAL_AUTHORIZATION="Bearer <workflow-service-token>"
export SERVER_ENVIRONMENT=dev
export LIGHT_WORKFLOW_CONFIG_MODE=local
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
LIGHT_WORKFLOW_CONFIG_MODE=local
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

The typed `server.environment` value validates the gateway service token's
`env` claim. `SERVER_ENVIRONMENT` remains a required bootstrap compatibility
input and must match it.
The workflow service's own `LIGHT_PORTAL_AUTHORIZATION` JWT must carry the same
`env` claim; startup rejects a missing or mismatched claim before that token can
be forwarded to a protected API in `X-Scope-Token`.
The same typed environment selects workflow Tool grants. A compatibility
`LIGHTAPI_ENVIRONMENT`, when present, must match; it no longer defaults to
`local`. `workflow.invocation.allowedCallerServiceIds` is the Config Server
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

`workflow.execution.maximumParallelism` is the service-wide hard ceiling for the number
of branches in a workflow fork. It defaults to 64 and must be between 1 and 64.
Both REST and event-driven workflow starts enforce this ceiling. A gateway or
Tool binding's `maximumParallelism` remains accepted on the wire for backward
compatibility, but it has no effect and is not enforced by `light-workflow`.
REST starts pin the configuration generation accepted with the invocation.
Legacy `WorkflowStartedEvent` projection applies the current generation when
the event is claimed because that historical event contract carries no accepted
configuration generation; this claim-time behavior is explicit until the event
schema gains a durable generation field.

Runtime refresh must target only `light-workflow/runtime-config`. A broad
`Reload All` request is rejected before any runtime module is changed. The
serialized refresh lock covers snapshot fetch through activation, and the exact
values document used to construct a candidate is digest-checked and persisted
with that candidate rather than reread from the shared cache path.

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
  execution remains disabled unless `workflow.runner.enabled=true` is in the
  active typed configuration.

The versioned workflow execution policy schema and its valid/invalid
conformance fixtures are published under
`crates/workflow-policy/schema/` and `crates/workflow-policy/fixtures/`.

Runner scheduling and terminal-result reconciliation use Controller's HTTPS
execution API. Configure `workflow.runner.executionApiUrl` and, when required,
`workflow.runner.executionApiCaCertFile`; the Workflow service token must carry
the exact Host, Workflow service ID, and `execution.invoke`. Controller alone
opens the `operations_execution_runtime` connection and owns sessions,
attempts, leases, approval evidence, inputs, provenance, and runtime audit in
`operations.execution_ops`. Workflow never holds its local transaction across
the API call, uses a deterministic request ID for restart replay, and
acknowledges a result only after Workflow state commits.

## Artifact object store

Runner artifact acceptance is fail-closed. When a terminal result declares an
artifact, `light-workflow` requires an S3-compatible object store and verifies
the runner's staging object before accepting the attempt. Configure it with
the standard AWS credential/workload-identity variables plus these Config
Server properties:

```yaml
workflow.artifact.s3Bucket: workflow-artifacts
workflow.artifact.prefix: light-workflow
workflow.artifact.retentionDays: 30
# Optional for MinIO or another S3-compatible service:
workflow.artifact.s3Endpoint: https://minio.example.net
# Development only:
# workflow.artifact.allowHttp: true
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
stale delete claims, and retains the database tombstone. When a runner supplies
a trusted provenance digest in its terminal evidence, Workflow binds that
digest into approval state. The provenance record itself is Controller-owned
execution evidence; Workflow does not query or update the execution store
directly.

## Trusted fixed-action providers

Phase 3 does not start a fixed-action provider worker inside Workflow because
that would restore direct execution-table access. Configuring either legacy
provider endpoint fails startup until the provider protocol is exposed through
a Controller-owned execution worker API. No platform or signing credential is
placed in workflow context, a runner, an agent, or a sandbox. The reserved
properties remain:

```yaml
workflow.fixedActions.repositoryUrl: https://repository-actions.example.net/v1/
workflow.fixedActions.releaseUrl: https://release-actions.example.net/v1/
```

Keep only the provider tokens in secret environment variables:

```bash
WORKFLOW_REPOSITORY_FIXED_ACTION_TOKEN=<service-to-service-token>
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

## Config Server bootstrap and recovery

The checked configuration inventory, dynamic resolver rules, three-identity
model, registration metadata contract, observability names, characterization
matrix, and equivalent local/remote fixtures are in
[`config-contract/`](config-contract/README.md). They pin current behavior and
Phase 1 authority decisions. Phase 1a now boots through the promoted Config
Server snapshot selected by `startup.yml` `host`, `serviceId`, and `envTag`.
The Config Server resolves that logical identity to its internal instance and
returns the selected host ID, snapshot ID, instance ID, and content digest as
provenance. Light Workflow rejects missing or invalid response metadata before
application state is constructed.

Managed mode is the default. It stages remote values, validates the complete
candidate, and atomically replaces `config-cache/light-workflow-lkg.json` with
owner-only permissions. During a later Config Server outage, only a cache bound
to the same Config Server authority, host, service, and environment and
verified by digest may start the service. A fresh managed
boot without a current snapshot/cache and a corrupt or cross-identity cache
fail closed. Tests that intentionally avoid Config Server must set:

```bash
LIGHT_WORKFLOW_CONFIG_MODE=local
```

Deployments mount one named cache volume per replica and inject the Config
Server bearer only through `LIGHT_PORTAL_AUTHORIZATION`; snapshots and the LKG
file contain no credential values.

Phase 1b resolves `workflow.yml` into one typed candidate before opening the
database pool or starting listeners and workers. Non-secret operational values
use `workflow.*` Config Server properties. Database URLs, service and provider
tokens, delegation secrets, object-store credentials, and mounted runner
profiles remain secret environment/provider/file inputs. The candidate rejects
invalid ranges and identity relationships together with property paths;
managed agent records also reject `literal:` API-key references. The typed
development exception `workflow.invocation.ignoreUserJwtExpiry` affects only
the forwarded user token. The workflow service scope token always enforces
both its environment and expiration.

Phase 2 runs the rule and invocation API through the shared Light Runtime Axum
transport. `GET /health` is process liveness; `GET /ready` is traffic
readiness and returns `503` while startup is incomplete, admission is draining,
or a critical background task has failed. The runtime closes application
admission, quiesces every workflow claimer/listener, drains accepted HTTP
requests, stops the listener, joins the managed tasks, and closes PostgreSQL in
that order. A startup failure after any task is registered unwinds the same
participants in reverse registration order.

Phase 3 registers managed deployments with controller-rs through the shared
Light Runtime registry client after the listener and workflow readiness
prerequisites are established. Registration carries the typed Cargo build
version and one complete `light.workflow.*` tag map containing the effective
configuration digest and snapshot identity, configuration source and refresh
time, readiness/degraded reasons, controller connection state, drain state,
and subsystem-specific capacity. Metadata updates replace the complete map;
they are also retained as reconnect registration state, so a reconnect cannot
restore stale values or create a second workflow-specific controller session.

`light.workflow.lifecycle.drainState=draining` is published before application admission
and the listener close. It is operational visibility, not a typed controller
routing instruction; deregistration remains the discovery-removal boundary.
An outage after successful startup is reported through controller metadata and
logs but does not bypass workflow authorization or stop already-authorized
execution. Managed dev/loc/installer deployments use fail-closed controller
startup (`server.startOnRegistryFailure: false`). Controller events and the
Portal `runtime_instance_t` projection persist the build version and complete
metadata map, and the Control Pane displays build, readiness, and drain state
after a controller or browser restart.

The v1 event projection remains one logical partition: consumer group
`workflow-engine-group`, topic `1`, partition `0`, `totalPartitions: 1`, and a
batch size of 10. Replicas contend on the same `consumer_offsets` row, so its
transaction lock serializes claims and a surviving replica resumes from the
committed `next_offset`; no event ownership is held only in process memory.
This provides failover but intentionally caps projection throughput at one
ordered partition. Add explicit partition assignment only after measured lag
shows that this serialized contract is insufficient, because changing
`totalPartitions` changes the offset-to-partition mapping and needs a migration
and replay plan.

Phase 4 adds live refresh for the six properties classified `reloadable` in
`config-contract/configuration-inventory.yml`. In Portal, open the running
Light Workflow instance in the Control Pane, choose **Modules**, select only
`light-workflow/runtime-config`, and invoke **Reload**. The authenticated
controller operation fetches the current promoted Config Server snapshot; it
does not accept property values in the request. All six values validate and
activate as one immutable generation. Accepted requests retain their captured
request/invocation policy, while executor worker capacity intentionally tracks
the current generation. A bad candidate or any restart-required difference
leaves the prior generation serving and is reported by `/ready`, controller
metadata, and the module reload result.

For rollback, set a previously reviewed snapshot current with the deployment's
`light-workflow-rust/rollback-current-snapshot.sh`, then reload the same single
module. If the review reports a restart-required property, restore the
snapshot and restart Light Workflow instead. The rollback script accepts an
existing snapshot ID for the configured Light Workflow instance and never
accepts or writes arbitrary property bodies. Config Server failure during a
refresh also leaves the prior generation active.

Run the deterministic gate with:

```bash
./scripts/run-light-workflow-config-controller-phase0-gate.sh
./scripts/run-light-workflow-config-controller-phase1a-gate.sh
./scripts/run-light-workflow-config-controller-phase1b-gate.sh
./scripts/run-light-workflow-config-controller-phase2-gate.sh
./scripts/run-light-workflow-config-controller-phase3-gate.sh
./scripts/run-light-workflow-config-controller-phase4-gate.sh
```

Pass a disposable PostgreSQL URL to include the origin-restart and fencing
characterization cases. The workspace CI already executes the deterministic
Rust contract test through `cargo test --workspace --locked`.
