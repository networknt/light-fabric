# Light-Workflow Runner

## Status

Proposed design.

`light-workflow-runner` is a tenant-side execution agent primarily introduced
for workflow tasks that must run near tenant systems, tenant repositories,
private tools, local gateways, sidecars, or sandboxed release workspaces. The
same controller, lease, fencing, and backend substrate can also execute
standalone agent turns or actions submitted by `light-agent`. It is not a
second workflow or agent engine and it must not consume workflow start events
or own interactive agent sessions.

The SaaS-owned `light-workflow` instance remains authoritative for workflow
subjects. `light-agent` remains authoritative for standalone agent sessions,
turns, and actions. Tenant runners register with `controller-rs`, receive
server-issued fenced execution leases, execute only the leased attempt, and
report normalized results for the authenticated origin service to reconcile.

For effectful work, the runner uses a capability-described `ExecutionBackend`
as defined in the
[Execution Backends And Sandbox Execution design](../product/light-workflow/sandbox-execution.md).
The backend may be a microVM sandbox, shared-kernel container, Kubernetes Job,
dedicated VM, host-integrated environment, or fixed external action. Backend
credentials, lifecycle calls, logs, and artifact transfer belong to the runner.
`light-workflow` must not implement competing direct backend protocols.

The interactive agent ownership and placement model is defined in
[Light-Agent Execution](light-agent-execution.md).

## Problem

For SaaS deployments, Light owns the main workflow control plane. Tenants may
run APIs, gateways, sidecars, deployers, and other services in their own
networks. Some workflow tasks need to execute inside those tenant environments
instead of inside the SaaS control plane.

Examples:

- release workflows running in a prepared VM or sandbox with many repositories
  checked out,
- command-line tasks that need local files or private repository access,
- build and test tasks that need tenant-specific toolchains,
- deployment tasks that need access to private clusters,
- MCP servers or sidecars running only in the tenant network,
- AI repair tasks that need to inspect and patch a local sandbox workspace.

Running multiple full `light-workflow` instances would create control-plane
ambiguity:

- more than one instance may see the same workflow start event,
- tenant-side config can be changed through environment variables or local
  `values.yml`,
- a tenant runtime could claim work outside its intended scope,
- workflow definition loading and event consumption become hard to audit,
- duplicate workflow starts require more complex idempotency and broker ACLs.

The platform needs a runner model that lets tenant-side services execute
approved tasks without letting them own workflow orchestration.

## Goals

- Keep one authoritative SaaS `light-workflow` orchestrator for workflow start
  events and workflow state.
- Add a tenant-side `light-workflow-runner` executable for command, sandbox,
  deployment, MCP, and local tool execution.
- Register tenant runners through `controller-rs`.
- Enforce task visibility with server-side leases, not runner-side local
  config.
- Support release runners in prepared VMs or sandboxes with checked-out repos
  and approved toolchains.
- Support per-tenant runner pools, execution profiles, capabilities, and
  network placement.
- Let `controller-rs` periodically audit effective runtime configuration.
- Reuse `workflow-core` task models and result contracts where possible.
- Keep the runner transport origin-neutral so workflow tasks and standalone
  agent turns can share execution infrastructure without sharing domain
  ownership.

## Non-Goals

- Do not create a second workflow orchestrator that consumes workflow start
  events.
- Do not let tenant runners load arbitrary workflow definitions from local
  config.
- Do not trust tenant-side environment variables or local `values.yml` as the
  enforcement boundary.
- Do not expose all workflow tasks to all registered runners.
- Do not let AI or command tasks bypass publish, signing, or human approval
  gates.
- Do not turn a standalone agent turn into a fake workflow task merely to use a
  runner.
- Do not let `controller-rs` or a runner advance workflow or agent domain
  state.

## Current Runtime Boundary

The current `light-workflow` executable starts the workflow event consumer, task
executor, and rule API in one process. The executor actively handles
control-plane task types such as `ask`, `assert`, `call`, `set`, and `switch`.

`workflow-core` already models `run.container`, `run.script`, `run.shell`, and
`run.workflow`. These task definitions are the right surface for runner-backed
execution, but they still need a runtime executor boundary.

This design keeps the workflow model shared and adds a separate runner
executable for effectful execution.

## Recommended Architecture

```text
Domain Event                         Interactive Client
  |                                      |
  v                                      v
light-workflow                       light-agent
  | workflow task                       | agent turn/action
  | origin-owned policy and attempt     | origin-owned policy and attempt
  +------------------+-------------------+
                     |
                     v
                controller-rs
  |
  | registration, fenced leases, heartbeat, quarantine
  v
light-workflow-runner
  |
  | approved execution and backend interaction
  v
ExecutionBackend
  |
  | declared isolation, lifecycle, resource, network, workspace,
  | credential, log, and artifact policy
  v
Tenant Runtime Environment
```

The split is:

- `light-workflow`: Authoritative orchestrator. It sees workflow start events,
  loads workflow definitions, persists immutable policy snapshots, creates task
  attempts, owns retry and cancellation decisions, and records state.
- `light-agent`: Authoritative orchestrator for standalone authenticated
  sessions, turns, and agent actions. It owns model-loop and memory state,
  creates action intent, and reconciles results without advancing workflow
  state.
- `controller-rs`: Runtime control plane. It authenticates runners, records
  runner capabilities, issues and renews fenced execution leases, rejects stale
  reports, audits runtime config, and quarantines mismatched runners.
- `light-workflow-runner`: Tenant-side execution agent. It claims only leased
  attempts, validates the effective policy and command template, executes in
  the approved environment, streams bounded logs, safely exports artifacts,
  and reports normalized results.
- `ExecutionBackend`: Backend-specific adapter used by the runner for
  capability discovery, effective-configuration validation, idempotent prepare
  and execute, inspection, cancellation, log cursors, artifact copy, optional
  checkpoints, and cleanup.
- Execution environment: A microVM sandbox, shared-kernel container,
  Kubernetes Job, dedicated VM, host-integrated environment, or fixed external
  action selected for the task's purpose and minimum isolation requirement.

The runner can run beside tenant APIs, gateways, sidecars, and deployers. It
may also run in a prepared release VM or sandbox with approved tools and
repository workspaces.

A local deployment may colocate these components, but it must retain the same
durable attempt, policy, lease, fencing, result, and audit contracts. Colocation
must not create a second execution model.

## Execution Origin And Subject

The runner wire contract and controller capacity queue are origin-neutral.
They carry a generic execution subject instead of requiring every execution to
be a workflow task:

```text
executionId
origin.service
origin.instance
subject.kind = workflow-task | agent-turn | agent-action
subject.id
subject.attempt
optional workflow or agent correlation
```

The authenticated origin service owns the domain state:

- `light-workflow` may create and reconcile `workflow-task` subjects;
- `light-agent` may create and reconcile `agent-turn` and `agent-action`
  subjects;
- `controller-rs` reserves capacity, transports leases, and stores fenced
  execution observations, but cannot complete a workflow task or agent turn;
- the runner executes a lease and cannot change origin-owned state.

Origin authorization is server-owned. A caller cannot select another origin
kind in the payload. Tenant, host, origin service, and allowed subject kinds
come from validated identity and registration.

Workflow and agent domain tables remain separate. Common scheduling,
execution-attempt, lease, backend, session, artifact, and runtime-audit records
may share the generic subject identity.

A runner-backed `agent_action_attempt_t` references the shared
`execution_attempt_t` row. Agent-domain tool, model-iteration, approval, budget,
and conversation fields remain outside the common runner table, matching the
separation between `task_info_t` and runner execution state.

### Origin Result Wakeup

The common `execution_attempt_t` row is the durable source of truth. The
controller transaction that conditionally stores a newly terminal result also
emits a versioned PostgreSQL `execution_result_ready_v1` notification. Its
bounded payload contains only attempt ID, authenticated origin, subject kind,
and correlation ID—never result bytes, tenant content, or authorization.

The named origin uses the notification only to wake its reconciler, reloads the
authoritative row, verifies origin/subject/fencing bindings, and conditionally
accepts the result into its own domain transaction. Every origin also performs
an indexed startup and periodic scan of unaccepted terminal attempts because
notifications can be missed, duplicated, or reordered. A later typed callback
may be another wakeup, but neither a callback nor the runner may directly
update workflow or agent domain tables.

The listener uses a dedicated PostgreSQL connection. On initial startup and
reconnect it establishes `LISTEN` first, then runs the catch-up scan, so a
terminal commit in that handoff window is either found by the query or queued
as a notification.

## Event Visibility

Workflow start events should be visible only to the SaaS-owned
`light-workflow` orchestrator.

Recommended flow:

1. A domain event is published.
2. The SaaS `light-workflow` consumer evaluates matching workflow definitions.
3. It creates one workflow instance per matching definition.
4. It creates tasks with runner requirements.
5. `controller-rs` exposes only eligible task leases to registered runners.
6. Runners execute leased tasks and return results.

This avoids duplicate starts and avoids tenant-side event subscription
authorization problems.

If a future deployment requires separate workflow clusters, route start events
by lane and enforce broker ACLs:

```text
workflow.start.main
workflow.start.release
workflow.start.deployment
workflow.start.tenant.<tenantId>
```

Even with event lanes, the workflow database should enforce idempotency on a
source-event key such as:

```text
tenant_id + source_event_id + workflow_definition_id
```

For the SaaS model, task leases are the cleaner boundary than exposing start
events to tenant runtimes.

## Runner Registration

A runner must register before it can claim work.

Registration should include:

```json
{
  "runnerId": "release-runner-01",
  "tenantId": "tenant-a",
  "hostId": "host-a",
  "runnerKind": "release",
  "runnerPools": ["release"],
  "executionProfiles": ["release-sandbox"],
  "capabilities": [
    "git",
    "maven",
    "cargo",
    "rootless-buildkit",
    "event-importer"
  ],
  "executionBackends": [
    {
      "backendId": "cube-prod-east",
      "kind": "microvm",
      "implementation": "cubesandbox",
      "version": "approved-version",
      "capabilityDigest": "sha256:...",
      "isolationBoundary": "microvm",
      "supportsUntrustedCode": true,
      "sessionScopes": ["task", "workflow"],
      "workspaceModes": ["ephemeral", "copy-on-write", "workflow"],
      "networkEnforcement": ["deny-by-default", "http-l7"],
      "credentialDelivery": ["brokered", "proxy-injected"],
      "lifecycle": ["inspect", "reconnect", "cancel", "destroy"]
    },
    {
      "backendId": "docker-sbx-local",
      "kind": "microvm",
      "implementation": "docker-sandboxes",
      "version": "approved-version",
      "capabilityDigest": "sha256:...",
      "isolationBoundary": "microvm",
      "supportsUntrustedCode": true,
      "sessionScopes": ["task", "workflow"],
      "workspaceModes": ["clone"],
      "networkEnforcement": ["deny-by-default", "http-l7"],
      "credentialDelivery": ["proxy-injected"],
      "containerEngineAccess": "private-daemon",
      "lifecycle": ["inspect", "reconnect", "cancel", "destroy"]
    },
    {
      "backendId": "toolbx-local",
      "kind": "host-integrated",
      "implementation": "toolbx",
      "version": "approved-version",
      "capabilityDigest": "sha256:...",
      "isolationBoundary": "host-integrated",
      "supportsUntrustedCode": false,
      "sessionScopes": ["none"],
      "hostExposure": [
        "home",
        "dbus",
        "devices",
        "network",
        "ssh-agent",
        "system-journal",
        "host-sockets"
      ]
    }
  ],
  "imageDigest": "sha256:...",
  "configHash": "sha256:...",
  "commandAllowlistHash": "sha256:...",
  "workspacePolicy": "release-workspace-v1",
  "workspaceChangePolicyDigests": ["sha256:..."],
  "trustBundleDigests": ["sha256:..."],
  "provenanceModes": ["slsa-provenance-v1-signed"],
  "localCleanup": {
    "watchdog": true,
    "durableJournal": true,
    "backendResourceScan": true,
    "policyDigest": "sha256:..."
  },
  "networkZone": "tenant-private",
  "version": "0.3.0"
}
```

`controller-rs` validates the registration against server-side runtime policy.
If accepted, it creates a runner session and issues short-lived credentials for
heartbeat and task claim operations.

Local runner config can request capabilities, but the server decides the
effective capabilities. A runner cannot claim work merely because it sets an
environment variable or local `values.yml` value.

Registration is an admission request, not attestation by itself. Backend
self-report does not establish a security boundary. Server-owned compatibility
records and conformance tests decide which capabilities are trusted. Server
policy must still constrain every lease, and the runner must prove the selected
backend, immutable template or image, command, resource, network, workspace,
host-exposure, workspace-change, credential, provenance, trust-bundle, and local
cleanup settings for each attempt. A runner without a healthy watchdog and
durable cleanup journal cannot claim backend-creating work. Unsupported or
unverifiable required controls fail closed.

## Execution Lease Model

The execution lease is the enforcement object. The runner should execute an
attempt only when it has a valid lease issued by the control plane. Workflow
correlation is present for a workflow subject; agent correlation is present for
an agent subject.

Lease example:

```json
{
  "executionId": "01970f5d-0000-7000-8000-000000000000",
  "leaseId": "01970f5d-0000-7000-8000-000000000001",
  "fencingToken": 17,
  "origin": {
    "service": "light-workflow",
    "instance": "workflow-main-east"
  },
  "subject": {
    "kind": "workflow-task",
    "id": "01970f5d-0000-7000-8000-000000000020",
    "attempt": 1
  },
  "tenantId": "tenant-a",
  "hostId": "host-a",
  "runnerId": "release-runner-01",
  "policySnapshotId": "01970f5d-0000-7000-8000-000000000010",
  "policyDigest": "sha256:...",
  "workflow": {
    "wfInstanceId": "release-2026.06.0",
    "taskId": "01970f5d-0000-7000-8000-000000000020",
    "wfTaskId": "build-java-products"
  },
  "operationType": "run.shell",
  "runnerPool": "release",
  "executionProfile": "release-sandbox",
  "profileVersion": 7,
  "capabilities": ["git", "maven"],
  "commandTemplateId": "light-fabric-release-build",
  "commandIdempotencyKey": "release-2026.06.0/build-java-products/1",
  "executionRequirements": {
    "minimumBoundary": "microvm",
    "allowedHostExposure": [],
    "workloadTrust": "untrusted"
  },
  "executionBackend": {
    "backendId": "cube-prod-east",
    "kind": "microvm",
    "implementation": "cubesandbox",
    "version": "approved-version",
    "capabilityDigest": "sha256:..."
  },
  "sandbox": {
    "templateId": "tpl-immutable-id",
    "templateDigest": "sha256:...",
    "sessionScope": "workflow",
    "workspaceMode": "copy-on-write"
  },
  "networkPolicy": "release-egress-v3",
  "trustBundleRef": "trust-bundle://enterprise-egress-v3",
  "trustBundleDigest": "sha256:...",
  "resourcePolicy": "release-build-medium-v1",
  "artifactPolicy": "release-artifacts-v2",
  "inputRefs": [
    {
      "kind": "skill-package",
      "id": "skill-package://coding/rust-review/7",
      "digest": "sha256:...",
      "size": 18432,
      "mountMode": "read-only"
    }
  ],
  "provenancePolicy": {
    "format": "slsa-provenance-v1",
    "mode": "signed",
    "policyDigest": "sha256:..."
  },
  "credentialRefs": [],
  "approvalRef": null,
  "deadlineAt": "2026-06-08T19:25:00Z",
  "environmentExpiresAt": "2026-06-08T19:25:00Z",
  "cleanupDeadlineAt": "2026-06-08T19:30:00Z",
  "expiresAt": "2026-06-08T19:10:30Z",
  "heartbeatIntervalSeconds": 15
}
```

Server-side validation must check:

- runner session is active,
- runner is not quarantined,
- tenant and host match,
- subject runner pool matches the registered pool,
- subject execution profile is allowed,
- required capabilities are a subset of effective runner capabilities,
- policy snapshot and digest match the active origin-owned subject,
- attempt number and fencing token match the active execution attempt,
- command template is approved,
- selected backend compatibility, workload trust, minimum isolation boundary,
  immutable template or image, host-exposure, network, resource, artifact,
  workspace, workspace-change, trust-bundle, provenance, lifecycle, local
  cleanup, and credential policies are supported,
- required runtime approval is valid and bound to the exact attempt inputs,
- execution and lease deadlines have not expired.

The runner reports execution start, logs, progress, and final result using the
lease. The control plane rejects reports that do not match the active attempt,
lease, and fencing token.

### Attempt And Fencing

Remote runner execution is at-least-once. Every execution uses a durable
attempt with a monotonically increasing attempt number and fencing token. The
lease is short-lived but renewable while the subject is active. Renewal proves
runner liveness; it does not extend the execution wall-clock deadline.

Start, progress, log, artifact, and result messages include the execution
origin, subject, attempt, lease ID, and fencing token. Result acceptance uses
compare-and-set semantics against the active attempt. An expired runner or late
backend result cannot overwrite a newer attempt or transition origin-owned
state.

If the runner loses contact after backend dispatch, the attempt becomes
`UNKNOWN`. The runner or control-plane reconciler inspects the backend
operation before the origin service decides to accept a result, wait, cancel,
retry, or require operator intervention. It must not assume that a transport
failure means the command did not run.

The subject `idempotencyKey` and lease `commandIdempotencyKey` are propagated to
the backend and external action where supported. Side-effecting command
templates must define an external idempotency or reconciliation contract before
automatic retry is allowed.

### Cancellation And Lease Loss

Cancellation and policy revocation fence the attempt before asking the runner
to stop. The runner cancels the backend operation, destroys or quarantines the
execution environment as required, revokes execution credential handles, and reports
cleanup state. A completion received after fencing is retained only as
diagnostic evidence.

If cleanup cannot be confirmed, the attempt enters `cleanup-pending` and an
orphan reconciler continues inspecting backend resources. Lease loss alone
does not prove that the command stopped.

### Local Watchdog And Disconnected Cleanup

The runner must be able to clean tenant-local resources without contacting
`controller-rs`. A supervisor separate from the execution worker writes a durable
local cleanup journal before backend preparation, stops new work when the
control-plane session is lost, and lets active work continue only until the
locally tracked lease expiry. A connectivity grace period cannot extend the
lease or execution deadline.

At lease expiry or the earlier execution or environment deadline, the supervisor
locally fences the attempt, revokes execution credential handles, cancels the
backend operation, and destroys or quarantines the environment. It uses a
monotonic deadline bounded by the authenticated absolute lease deadline.
Backend resources carry owner and expiry tags and use a native TTL where the
backend supports one, so cleanup does not depend on the runner host restarting.

Runner startup and periodic sweeps replay incomplete journal records and inspect
tagged resources with bounded cleanup backoff. Reconnection reports outcome and
cleanup evidence, but it cannot make an expired result valid. External actions
that may already have taken effect remain `UNKNOWN` and are reconciled rather
than blindly retried. The detailed journal and watchdog contract is defined in
the [Execution Backends And Sandbox Execution design](../product/light-workflow/sandbox-execution.md#disconnected-runner-watchdog).

### Capacity Scheduling And Claim Backoff

`controller-rs` keeps execution subjects with no eligible slot in a bounded,
per-tenant fair `PENDING_CAPACITY` queue. `light-workflow` retains
authoritative workflow-task state and `light-agent` retains authoritative
agent-turn/action state. The controller atomically reserves runner and backend
capacity and returns a short-lived reservation token; idempotent, fenced
attempt creation and lease issuance bind that token. Temporary saturation
therefore does not consume an origin retry, create an execution environment, or
cause every runner to race for the same subject.

Claims use long polling or server push. An empty claim or temporary backend
capacity response includes `retryAfter`; clients apply capped exponential
backoff with jitter, and the controller wakes only a bounded number of eligible
waiters when capacity returns. Hard quota or policy failures are terminal
admission denials until configuration changes. Queue timeout, origin deadline,
cancellation, and policy revocation remove pending work without ever
dispatching it.

## Task Routing

`light-workflow` should execute pure control-plane tasks locally:

```text
ask
assert
set
switch
context merge
workflow branching
workflow persistence
approved internal call tasks
```

`light-workflow-runner` should execute effectful or tenant-local tasks:

```text
run.shell
run.script
run.container
call.mcp to tenant-local servers
deployment commands
release build and test commands
AI repair with filesystem access
browser automation
external tool processes
```

Some `call.*` tasks can run on either side. The routing decision should come
from effective task policy:

| Task | Default Runtime | Notes |
| --- | --- | --- |
| `call.http` internal SaaS API | `light-workflow` | Use host-side service credentials. |
| `call.http` tenant-private API | runner | Needs tenant network access. |
| `call.mcp` approved SaaS gateway | `light-workflow` | Gateway enforces tool access. |
| `call.mcp` tenant-local server | runner | Local sidecar or private MCP server. |
| `call.agent` no tools | `light-workflow` | Bounded model call. |
| `call.agent` with file/tools | runner | Requires sandbox/tool policy. |

## Agent Call Placement

Workflow agent calls need an explicit placement decision. The same workflow can
use more than one agent execution mode, but the placement must come from
server-side policy and task metadata, not tenant-side local config.

Use three agent execution modes.

### Native Workflow Agent

Native `call: agent` stays in the SaaS-owned `light-workflow` process. This is
the current bounded agent task model: `light-workflow` resolves the portal
agent, skill, and tool metadata, builds a constrained prompt from workflow
context, calls the configured model provider, validates structured output, and
continues the workflow.

Use native workflow agents for bounded reasoning:

- classify a request or command result,
- summarize API responses or logs,
- choose a workflow branch,
- draft a customer-facing explanation,
- decide whether human review is required,
- produce JSON output that must match a schema.

Native workflow agents should not receive filesystem access, local network
access, release secrets, or dynamic tool execution. API orchestration should
remain explicit workflow tasks such as `call.http`, `call.mcp`, `assert`,
`switch`, and `ask`.

By default, native workflow agents use SaaS-approved model providers and model
credentials managed by the Light control plane. Tenant-private repository
content, tenant-local logs, local files, and private network data should not be
sent to this path unless the tenant policy explicitly allows it.

### Runner Agent

Runner agents execute through `light-workflow-runner` under a server-issued
execution lease. Use this mode when the agent needs access to tenant-local
state or effectful tools:

- checked-out repositories,
- command output plus working directory inspection,
- private tenant network access,
- local MCP servers,
- sandbox tools,
- AI repair of source code,
- test reruns,
- branch or pull-request creation.

The origin service still creates the workflow task or standalone agent action
and records the result. `controller-rs` issues a lease only to a runner whose
effective capabilities, runner pool, execution profile, command allowlist,
workspace policy, and audit state match the execution requirements.

Runner agent lease example:

```json
{
  "executionId": "01970f5d-3333-7000-8000-000000000001",
  "origin": {
    "service": "light-workflow",
    "instance": "workflow-main-east"
  },
  "subject": {
    "kind": "workflow-task",
    "id": "01970f5d-3333-7000-8000-000000000020",
    "attempt": 1
  },
  "operationType": "call.agent",
  "agentPlacement": "runner",
  "runnerPool": "release",
  "executionProfile": "release-sandbox",
  "profileVersion": 7,
  "executionRequirements": {
    "minimumBoundary": "microvm",
    "allowedHostExposure": [],
    "workloadTrust": "untrusted"
  },
  "executionBackend": {
    "backendId": "docker-sbx-local",
    "kind": "microvm",
    "implementation": "docker-sandboxes",
    "version": "approved-version",
    "capabilityDigest": "sha256:..."
  },
  "sandbox": {
    "sessionScope": "task",
    "isolationClass": "agent-call",
    "workspaceMode": "clone"
  },
  "modelProviderScope": "tenant",
  "modelAccessMode": "brokered-proxy",
  "modelProxyRef": "tenant-model-proxy-eastus",
  "workloadIdentityRef": "attempt://model-access",
  "dataBoundary": "tenant-network",
  "runtimeToolManifestDigest": "sha256:...",
  "allowedTools": [
    {
      "toolRef": "runner://command/cargo-test",
      "modelAlias": "cargo_test",
      "schemaDigest": "sha256:...",
      "capability": "command.cargo.test"
    }
  ],
  "workspaceAccess": "copy-on-write-release-workspace",
  "workspaceBaseRevision": "git:...",
  "workspaceChangePolicyId": "agent-source-only-v1",
  "workspaceChangePolicyDigest": "sha256:...",
  "networkPolicy": "release-egress",
  "credentialPolicy": "brokered-task-scoped",
  "maxRepairAttempts": 2,
  "requiresHumanApprovalBefore": ["publish", "sign", "tag"]
}
```

The runner agent can inspect files and propose or apply bounded patches inside
the approved workspace. It must not publish artifacts, sign releases, push
final tags, read unrestricted secrets, or expand its own permission scope.

By default, runner agents use tenant-approved model providers and tenant-owned
credentials. This keeps private workspace data and private network context
inside the tenant boundary and avoids exposing SaaS model credentials to
tenant-side runtimes.

Runner-local tools do not appear in `light-gateway` `tools/list`. Before worker
startup, the controller intersects catalog entries placed on the runner with
the server-approved runtime compatibility record, execution profile, lease
`allowedTools`, and immutable runtime-tool manifest. The worker may narrow that
set using live local availability or sandbox-local MCP `tools/list`, but cannot
add authority. Each model alias remains bound to one stable internal tool
reference, schema digest, placement, and dispatcher; cross-placement name
collisions fail closed. Broker/control sockets and backend lifecycle operations
are never exposed as tools.

### Agent Workspace Change Boundary

The lease for a write-capable agent includes the immutable base commit or tree
and a server-owned `workspaceChangePolicyId` and digest. The policy intersects
allowed paths with protected-path denies and limits file count, bytes, types,
creation, deletion, rename, mode, submodule, nested-repository, and binary
changes. Workflow metadata and agent output cannot weaken it.

The default policy denies CI/CD definitions, reusable automation, workflow
definitions, `CODEOWNERS` and approval policy, `.git` internals and hooks, and
release, publish, signing, deployment, credential, runner, and execution-policy
configuration. Repository-specific equivalents are added by operator policy.

Write interception inside the sandbox is defense in depth. After execution, a
trusted runner component computes the authoritative diff from the immutable
base in a fresh trusted checkout without repository-provided hooks or mutable
Git configuration. It normalizes case, Unicode, and separators according to
repository rules, detects link and rename tricks, and creates an immutable
canonical patch whose digest is checked. A violation fails with
`workspace_change_denied`; no branch, pull request, artifact, publish, or
signing action may consume the patch.

The agent receives no push credential. Branch or pull-request creation is a
separate fixed action over the immutable accepted patch and its policy result,
never the mutable agent workspace. Even an accepted agent patch remains
untrusted: tests run under the same isolation, and a release rebuilds from the
reviewed and merged immutable commit rather than publishing artifacts directly
from the repair workspace.

### Runner Agent Execution Isolation

The runner itself is a tenant-side execution agent. For stronger isolation, the
runner can launch the agent task inside a separate environment such as Cube
Sandbox, Docker Sandboxes, a dedicated VM, or a Kubernetes Job using an
approved runtime class. This should be a tenant-selectable policy because the
runner is deployed in the tenant namespace, but the effective choice must still
meet the server-owned minimum boundary and be recorded in the execution lease.

Recommended isolation levels:

| Session Scope | Isolation Class | Use Case | Default Policy |
| --- | --- | --- | --- |
| `none` | bounded-runner | Model call with no tools, files, or private-network mutation | Allowed only for explicitly approved low-risk profiles |
| `workflow` | release-build | Build, test, or diagnosis sharing one checkout and cache | Useful for release workflows |
| `agent-session` | interactive-agent | Coding workspace reused across authenticated turns | Explicit TTL and identical principal/policy/base required |
| `task` | agent-call | AI repair, generated patches, dynamic tools, or untrusted scripts | Preferred for high-risk agent tasks |
| `task` | publish | Fixed publish or signing action over immutable artifacts | Required for high-value credentials |

Backend purpose and execution session scope are separate. Recommended defaults
are:

| Execution need | Minimum boundary | Candidate backend | Important constraint |
| --- | --- | --- | --- |
| Trusted local development | `host-integrated` | Runner operating in Fedora Toolbx | Toolbx is not a sandbox and cannot run untrusted or secret-bearing tasks |
| Trusted CI, tests, or packaging | `shared-kernel-container` | Rootless Docker or Podman; ordinary Kubernetes Job | No privileged mode, host namespaces, or host container-engine socket |
| Autonomous agent or untrusted code | `microvm` | Cube Sandbox; Docker Sandboxes; Kubernetes only with an approved stronger runtime | Docker Sandboxes use clone workspace mode; no fallback to a shared-kernel container |
| Privileged or long-running tenant work | `dedicated-vm` | Approved tenant-dedicated VM | Pin and attest the image, network, identity, limits, and teardown policy |
| Publishing, signing, or deployment | `external-service` | Fixed typed action or dedicated service | Accept immutable inputs; do not expose a general shell with release credentials |

For a release workflow, the runner should usually orchestrate a separate task
sandbox with the `agent-call` isolation class for AI repair. The runner provides
only the leased copy-on-write workspace, approved tools, network policy, and
opaque credential handles allowed by the task policy. It collects bounded logs,
artifacts, patches, and structured output, then destroys the sandbox. Freezing
or checkpointing is allowed only when retention policy permits it and no raw
credential entered the sandbox.

For a reused workflow or agent session, effective expiry is the earliest of
the origin session idle/max expiry, execution-session policy, credential or
broker grant expiry, and backend-native TTL. Closing, revoking, or expiring the
origin session creates a durable idempotent common cleanup request in the same
origin transaction. `controller-rs` fences and cancels active attempts and
dispatches cleanup; the runner destroys the physical session and records
evidence. Cleanup retries across restarts. Backend-native TTL is the final
fail-safe, not the expected way to reclaim an abandoned session.

Action ownership and session retention are separate. Ending an action lease
removes executable authority, broker access, and action credentials. It does
not by itself delete a compatible reused session. An origin may create a
durable `IDLE_APPROVAL_HOLD` for a non-secret workspace with an explicit hold
ID, reason, policy digest, `holdUntil`, checkpoint/patch evidence, and cost
policy. The runner pauses or checkpoints where supported. The hold cannot
extend the session idle/fixed maximum or survive origin close, revocation,
policy mismatch, or cleanup request. Controller and runner reconcilers use this
session state rather than treating zero active attempts as abandonment.

This creates a layered boundary:

```text
SaaS light-workflow or light-agent
  -> controller-rs fenced execution lease
  -> tenant light-workflow-runner
  -> ExecutionBackend
  -> task execution environment, isolationClass=agent-call
  -> model, tools, files, network
```

Tenants may choose Cube Sandbox, Docker Sandboxes, dedicated VM isolation, an
approved Kubernetes runtime, or no additional environment for explicitly
trusted profiles. Fedora Toolbx and ordinary shared-kernel containers must not
satisfy a microVM requirement. Runner registration advertises supported
execution backends, isolation boundaries, session scopes, workspace modes,
host exposures, and enforcement capabilities. If the approved compatibility
record cannot satisfy the task requirements, `controller-rs` must not issue the
lease; it must not silently choose a weaker backend.

Local runner config can select among tenant-approved profiles, but it cannot
weaken a task requirement. The lease contains the final effective
`executionBackend` identity, implementation, version, capability digest,
`sandbox.sessionScope`, `sandbox.isolationClass`, immutable template or image,
workspace, host-exposure, network, resource, tool, artifact, lifecycle, and
credential policy. Heartbeat, backend inspection, and audit snapshots should
prove the runner is still operating under that profile.

### Execution Backend Boundary

Backend-specific execution lives behind `ExecutionBackend` in the runner, not
in `light-workflow`. The core operations are capability discovery,
effective-configuration validation, idempotent environment preparation,
environment and operation inspection, reconnect, idempotent execution,
resumable logs, cancellation, safe artifact copy, and cleanup. Checkpoint and
session operations are optional capabilities rather than assumptions every
backend must emulate. Backends also report measured execution evidence where
supported; the trusted runner or control-plane attestor, not the tenant task,
constructs the final provenance statement.

The runner fails closed when the backend cannot enforce a required isolation,
host-exposure, resource, network, credential, lifecycle, or inspection control.
A backend timeout or
transport error is not automatically a command failure; the runner records an
unknown outcome and reconciles it through backend inspection.

### Trusted Input And Skill-Package Staging

Immutable context, workspace bases, trust bundles, and skill packages are
resolved into digest-bound input records before dispatch. The trusted runner,
outside the sandbox payload boundary, downloads them with runner authority,
checks kind, size, digest, signature/provenance where required, and archive
safety, and stages them before backend creation. The backend exposes only the
selected bytes as read-only mounts with `nodev`, `nosuid`, and `noexec` unless
an approved entrypoint requires execution.

`light-agent-worker` may revalidate the mounted manifest, but neither it nor
generated code downloads a package or receives artifact-store credentials.
Input verification or staging failure prevents sandbox start. Staging paths
are attempt/session scoped, journaled without secrets, and removed through the
same idempotent cleanup contract as the backend environment.

The complete policy, lifecycle, Cube production baseline, artifact, secret,
and failure contracts are defined in the
[Execution Backends And Sandbox Execution design](../product/light-workflow/sandbox-execution.md).

### Agent Service

Containerized `light-agent` services should be invoked explicitly. They are the
right runtime for interactive or independently scaled agents:

- chat and session memory,
- dynamic `tools/list` and `tools/call` loops,
- long-lived specialist agents,
- independently deployed model/tool runtime,
- local catalog caching.

Do not silently change native `call: agent` to call a containerized
`light-agent` service. Use an explicit contract such as `call: agent-service`
or `call: agent` with `mode: service` so operators can audit which runtime path
was used.

When an interactive `light-agent` session needs local execution, it submits an
`agent-turn` or `agent-action` execution subject through the same controller
and runner substrate. It does not create a fake workflow task. Session, turn,
tool authorization, memory, and model-provider rules remain governed by the
[Light-Agent Execution design](light-agent-execution.md).

For a workspace-aware coding or external-agent turn, the runner starts the
small sandbox-side `light-agent-worker` with a pinned runtime adapter. It does
not start the public `light-agent` service inside the sandbox. The worker owns
only the leased local loop and normalized event stream; light-agent remains the
agent-domain authority.

### Model Provider Boundary

Agent placement and model-provider placement should be decided together.

Recommended defaults:

```text
native call: agent in SaaS light-workflow
  -> SaaS-approved model provider
  -> SaaS workflow context data boundary

leased runner agent in tenant workflow runner
  -> tenant-approved model provider
  -> tenant network/workspace data boundary

containerized light-agent service
  -> service-owned or tenant-approved model provider
  -> explicit service data boundary
```

The default SaaS model is useful for bounded reasoning over workflow-safe
context, such as classification, summaries, branch decisions, and structured
JSON output. It should not be the default path for tenant-local source code,
private command logs, local files, or private network data.

The default runner model is useful when the task needs tenant-local context.
The runner owns an attempt-scoped model broker outside the untrusted payload
boundary. It binds a protected local channel to the subject, adapter, approved
model, data-boundary and policy digests, token/cost budget, rate, audience, and
expiry. Provider keys and reusable proxy bearer tokens never appear in a
sandbox environment variable, argument, prompt, workspace, or persistent file.
SaaS model credentials must not be sent to tenant runners.

The control plane should still make this policy-driven instead of hard-coding
it. Some tenants may require every agent call, including bounded summaries, to
use their own provider or regional model endpoint. In that case, the workflow
task should be routed to a runner or to an approved tenant model gateway even
if the reasoning itself is small.

Lease examples:

```json
{
  "agentPlacement": "workflow",
  "modelProviderScope": "saas",
  "modelProviderRef": "light-managed-default",
  "credentialRef": "saas-secret://llm-provider",
  "dataBoundary": "saas-workflow-context"
}
```

```json
{
  "agentPlacement": "runner",
  "modelProviderScope": "tenant",
  "modelAccessMode": "brokered-proxy",
  "modelProxyRef": "tenant-model-proxy-eastus",
  "workloadIdentityRef": "attempt://model-access",
  "dataBoundary": "tenant-network"
}
```

The sandbox worker receives only a runner-created preconnected descriptor,
peer-credential-checked Unix-domain socket, vsock, or backend-equivalent local
channel. A socket path is not sufficient authority: the broker authenticates
the peer and attempt and independently enforces model and budget policy. The
worker/runtime and generated payload use separate identities and process/mount
namespaces; ptrace and cross-process `/proc` access are denied, and the worker
does not pass its broker descriptor to child payloads. An adapter that requires
an extractable provider key is ineligible for an untrusted runner profile.

Recommended placement rule:

```text
bounded reasoning over workflow context -> native call: agent in light-workflow
agent needs one isolated local effect -> leased agent-action
workspace-aware coding or external agent loop -> light-agent-worker in leased agent-turn sandbox
interactive session or dynamic tool loop -> containerized light-agent service
```

For release workflows, use native `call: agent` to summarize and classify a
failed command. Use a runner agent for repo inspection, patch generation, test
rerun, and pull-request creation. Human approval remains required before
publish, signing, or final tag creation, but approval waiting is a durable
`light-workflow` orchestration state and never an active runner lease.

## Effective Policy

Workflow definitions and tasks can request runner execution through metadata,
but the control plane computes the effective policy.

Workflow-level example:

```yaml
document:
  dsl: "1.0.3"
  namespace: release
  name: java-release
  version: "0.1.0"
  metadata:
    lightWorkflow:
      runner:
        runnerPool: release
        capabilities:
          - git
          - maven
          - rootless-buildkit
      security:
        schemaVersion: 1
        executionProfile: release-sandbox
        profileVersion: 7
        placement: runner
        isolation:
          minimumBoundary: microvm
          allowedHostExposure: []
          workloadTrust: untrusted
        sandbox:
          sessionScope: workflow
        workspace:
          mode: copy-on-write
```

Task-level example:

```yaml
do:
  - build-java:
      run:
        shell:
          command: light-release-build
          arguments:
            - "${ .release.version }"
      metadata:
        lightWorkflow:
          runner:
            runnerPool: release
            commandTemplateId: light-fabric-release-build
          security:
            isolation:
              minimumBoundary: microvm
            sandbox:
              sessionScope: workflow
```

Runtime policy resolution:

1. Operator-approved immutable profile definitions set the base allowed
   commands, workload trust, minimum isolation, host exposure, execution
   backends, templates or images, networks, resources, mounts, session scopes,
   workspace modes and change policies, trust bundles, model-provider scopes,
   data boundaries, artifact provenance, local cleanup, and credentials.
2. SaaS service policy intersects the profiles and approved backend
   compatibility records available in the deployment.
3. Tenant policy further restricts the allowed set.
4. The workflow requests one profile and immutable version.
5. Task metadata may request stricter isolation or a subset of capabilities;
   it cannot downgrade operator-derived workload trust.
6. `light-workflow` persists the effective workflow policy snapshot and derives
   an effective task-policy digest for each attempt.
7. `controller-rs` validates the registered runner and selected backend against
   the server-owned compatibility record and capability digest.
8. The fenced execution lease contains the selected backend identity and final
   allowed execution scope.

For an agent origin, `light-agent` performs the analogous immutable
agent/turn/action policy resolution described in
[Light-Agent Execution](light-agent-execution.md). The controller and runner
consume the same final execution-policy fields without taking ownership of how
the origin derived them.

Policy merging is field-specific. Allowlists intersect, explicit denies win,
numeric limits use the lowest permitted maximum, and task-scoped isolation may
strengthen workflow-scoped isolation. The selected backend must meet the
minimum boundary and every required capability; there is no fallback from a
microVM to a shared-kernel or host-integrated environment. Backend and template
choices must be members of the approved compatibility set. A task cannot weaken
the effective policy.

The policy snapshot, execution session, execution attempt, backend operation, and
lease must use dedicated runtime tables. They must not be stored in mutable
workflow or agent context. Profile versions and backend compatibility records
are immutable; emergency revocation fences new and active attempts and records
the reason.

## Runtime Configuration Audit

Tenant-controlled local configuration cannot be the source of truth. A runner
can load local config for its own startup, but the server must verify and audit
the effective runtime state.

`controller-rs` should audit at three points.

### Startup Admission

On registration, the runner reports:

- binary version,
- image digest or VM image ID,
- effective config hash,
- command allowlist hash,
- enabled execution profiles,
- runner pools,
- mounted workspace paths,
- supported execution session scopes, isolation boundaries, and isolation
  classes,
- backend IDs, kinds, implementations, versions, capability digests, and
  server-approved enforcement capabilities,
- host exposures, workspace modes, workspace-change policy digests,
  container-engine access, and immutable template or image IDs and digests,
- local watchdog and cleanup-journal health and policy digest,
- trust-bundle digests and supported language-runtime adapters,
- provenance formats, modes, and trusted attestor identity where applicable,
- allowed model provider scopes,
- network zone,
- resource, artifact, and credential-delivery policies,
- host and tenant identity.

`controller-rs` compares this report with approved server-side policy before
allowing claims.

### Heartbeat

Each heartbeat should include:

```json
{
  "runnerId": "release-runner-01",
  "sessionId": "01970f5d-1111-7000-8000-000000000001",
  "status": "ready",
  "configHash": "sha256:...",
  "commandAllowlistHash": "sha256:...",
  "imageDigest": "sha256:...",
  "watchdog": {
    "status": "healthy",
    "lastSweepAt": "2026-06-08T18:59:45Z",
    "cleanupPending": 0,
    "policyDigest": "sha256:..."
  },
  "activeAttempts": [
    {
      "leaseId": "01970f5d-0000-7000-8000-000000000001",
      "attempt": 1,
      "fencingToken": 17,
      "policyDigest": "sha256:...",
      "backendId": "cube-prod-east",
      "backendOperationId": "backend-op-123",
      "leaseExpiresAt": "2026-06-08T19:00:30Z"
    }
  ],
  "timestamp": "2026-06-08T19:00:00Z"
}
```

If a hash changes unexpectedly, the controller marks the runner suspicious and
stops issuing new leases.

### Periodic Deep Audit

Periodically, `controller-rs` should request an effective runtime snapshot from
the runner and compare it with the approved policy. For high-risk runners, the
snapshot should include command allowlist, immutable sandbox template, mount
list, host exposure, workspace mode and change policy, resource, network, and
trust-bundle policy, backend effective configuration, artifact and provenance
policy, local cleanup journal health, tagged-resource scan result, and
credential binding names without values. Where the backend permits it, the
control plane should compare runner claims with backend inspection rather than
relying only on runner self-reporting.

On mismatch:

1. Mark the runner as `quarantined` and stop issuing new leases.
2. Fence active task attempts so late results cannot transition workflows.
3. Revoke claim and task credentials.
4. Request cancellation and backend cleanup for affected operations.
5. Emit an append-only runtime audit event.
6. Create an operator task when outcome or cleanup remains unknown.

Audit is not the only enforcement mechanism. It detects drift after admission.
The fenced task lease is the primary runtime authorization boundary, while the
backend must independently enforce its declared isolation, resource, network,
workspace, lifecycle, and credential policy.

## Release Runner Mode

A release runner is a specialized `light-workflow-runner` profile.

It can run in:

- an approved dedicated VM,
- a Cube Sandbox or Docker Sandboxes microVM,
- a Kubernetes Job with a recorded runtime class and node policy,
- a rootless shared-kernel container for explicitly trusted tasks,
- a controlled bare-metal or Toolbx environment for trusted local helper tasks.

The last two options are operational environments, not substitutes for a
microVM boundary. A runner using them cannot claim untrusted-code, isolated
agent, publish, signing, or secret-bearing tasks unless a separate eligible
backend performs that task.

Recommended default for release workflows:

- one workflow-scoped sandbox or VM workspace for checkout, build, test, and
  package steps,
- task-scoped `agent-call` isolation for AI repair, source inspection,
  generated patches, and test reruns driven by an agent,
- immutable artifact export through controlled storage with trusted-side
  hashing and trusted-side in-toto/SLSA provenance,
- server-owned protected-path policy plus trusted post-export diff validation
  for every agent patch,
- a task-scoped fixed publish action or separate release service for publishing,
- an external signing service or task-scoped fixed signing action,
- human approval bound to the exact artifact digest, release target, version,
  command template, policy snapshot, and expiry,
- clean checkout inside the runner rather than writable host repository mounts,
- AI repair limited to sandbox workspace changes, with branch or PR creation
  performed by a separate fixed action,
- a clean release rebuild from the reviewed and merged immutable commit rather
  than publishing an artifact directly from an agent-repair workspace.

Writable host mounts should be avoided for AI repair and release commands. If
host repositories must be mapped, default to read-only mounts and copy the repo
into a runner-owned working directory before mutation.

Publish and signing actions must not execute arbitrary scripts from the mutable
build workspace. They consume only immutable artifact records and use
operator-owned command templates. They verify artifact provenance and approval
bindings before use. Prefer brokered short-lived identity or backend-side
credential injection so the raw credential never enters arbitrary workflow
code. Per-task isolation limits exposure but does not make untrusted code safe
to receive a release token.

Do not mount a host Docker socket into tenant-authored runners or sandboxes.
Container image builds use an approved rootless builder or remote build service
with pinned builder and base-image digests.

## Approval Ownership And Lease Handoff

Human approval is owned entirely by the authenticated origin service:
`light-workflow` for workflow tasks and `light-agent` for standalone agent
actions. The runner never polls a person. Before an approval wait, the origin
commits any known result and immutable evidence, terminalizes the current
attempt, ends its **action lease**, closes its model-broker channel, and revokes
task credentials. A task-scoped environment is cleaned.

For a reusable non-secret session workspace, `WAITING_APPROVAL` may coexist
with the distinct bounded `IDLE_APPROVAL_HOLD` described above. The hold is not
an action lease, carries no executable authority, is preferably paused or
checkpointed, and consumes observable retained-resource quota. If a safe hold
cannot be established, export an immutable approved patch/checkpoint and clean
the environment; important uncommitted work must not depend only on a live
sandbox.

The origin transaction that enters `WAITING_APPROVAL` also persists exactly one
session disposition—cleanup or policy-valid bounded hold. If origin and common
session state later use separate databases, an idempotent transactional outbox
provides that handoff. A session reconciler must never observe an ended action
lease without the durable disposition and guess whether to delete the
workspace.

When policy knows approval is required before execution, the origin records
the bound intent but creates no common execution attempt. If a running runtime
discovers an approval boundary, it returns a known `approval_required` terminal
result and its attempt is fenced and cleaned or explicitly checkpointed under
non-secret retention policy.

After approval, the origin revalidates the exact operation, arguments,
artifacts/provenance where applicable, destination, policy digest, expiry, and
single-use nonce. It consumes the approval into a new numbered domain attempt
and a new common `execution_attempt_t`; `controller-rs` issues a fresh lease,
monotonic fencing token, and fresh task-scoped grants. The pre-approval attempt,
lease, backend handle, and grants remain immutable and cannot be reused.
Rejection or expiry changes only origin orchestration state and dispatches no
runner work.

If the held physical workspace still exists after approval, the new action may
reuse it only after principal/base/runtime/policy/expiry and cleanup-state
revalidation. Otherwise it starts in a fresh environment and restores only a
verified policy-permitted checkpoint or patch.

A non-secret workflow session may be checkpointed or retained during approval
only under explicit maximum-lifetime, cost, and retention policy; approval must
not depend on it. The default release flow cleans the build environment after
export. An unknown prior side effect is reconciled before approval can authorize
another attempt.

## Runner API

The first runner API can be small.

```text
POST /runner/register
POST /runner/heartbeat
POST /runner/claim
POST /runner/execution/{leaseId}/started
POST /runner/execution/{leaseId}/renew
POST /runner/execution/{leaseId}/progress
POST /runner/execution/{leaseId}/log
POST /runner/execution/{leaseId}/complete
POST /runner/execution/{leaseId}/fail
POST /runner/execution/{leaseId}/unknown
POST /runner/execution/{leaseId}/cancelled
POST /runner/execution/{leaseId}/cleanup
POST /runner/execution-session/{executionSessionId}/hold
POST /runner/execution-session/{executionSessionId}/resume
POST /runner/execution-session/{executionSessionId}/cleanup
POST /runner/audit-snapshot
POST /runner/drain
```

`controller-rs` can expose these APIs directly or mediate them over its
existing persistent connection model. For private tenant networks, outbound
runner registration and polling is preferable to inbound SaaS calls into the
tenant environment.

The claim response should include only the origin-neutral subject envelope and
payload needed for execution, not a full workflow definition, agent session,
or conversation history. `/runner/claim` supports long polling and an empty
response includes `retryAfter`; the runner applies capped exponential backoff
with jitter. A subject waiting for capacity has no lease and is not returned
until the controller has atomically reserved an eligible slot.

Every execution API request includes a unique message ID, origin, subject,
attempt number, lease ID, and fencing token. Repeated delivery of the same
message is idempotent. Lease renewal extends execution ownership only up to the
execution deadline. Cancellation can be delivered through the persistent
connection or returned from heartbeat and renewal calls.

There is no runner API for waiting on human approval. Approval is handled by
the origin service, and only a newly created post-approval attempt appears
through `/runner/claim`. Session `hold` and `resume` are idempotent lifecycle
commands from the controller; they contain no human decision and cannot create
or renew an action lease. They carry the session state version/fence, policy
digest, bounded `holdUntil`, and checkpoint/patch policy.

## Command Result Contract

Runner results should use a normalized command result so `light-workflow`,
`light-agent`, human tasks, AI diagnosis, and audit do not depend on raw
console parsing.

```json
{
  "executionId": "01970f5d-0000-7000-8000-000000000000",
  "leaseId": "01970f5d-0000-7000-8000-000000000001",
  "fencingToken": 17,
  "origin": {
    "service": "light-workflow",
    "instance": "workflow-main-east"
  },
  "subject": {
    "kind": "workflow-task",
    "id": "01970f5d-0000-7000-8000-000000000020",
    "attempt": 1
  },
  "workflow": {
    "taskId": "01970f5d-0000-7000-8000-000000000020",
    "wfTaskId": "build-java-products"
  },
  "runnerId": "release-runner-01",
  "policyDigest": "sha256:...",
  "commandTemplateId": "light-fabric-release-build",
  "backendId": "cube-prod-east",
  "backendKind": "microvm",
  "backendImplementation": "cubesandbox",
  "backendVersion": "approved-version",
  "backendOperationId": "backend-op-123",
  "status": "failed",
  "outcome": "known",
  "exitCode": 1,
  "startedAt": "2026-06-08T19:10:00Z",
  "completedAt": "2026-06-08T19:18:30Z",
  "summary": "Maven test failure in db-provider",
  "stdoutRef": "artifact://release/2026.06.0/build/stdout.log",
  "stderrRef": "artifact://release/2026.06.0/build/stderr.log",
  "artifacts": [
    {
      "artifactId": "01970f5d-0000-7000-8000-000000000030",
      "name": "surefire-reports.zip",
      "sha256": "sha256:...",
      "size": 42000,
      "storeUri": "artifact://release/2026.06.0/build/surefire-reports.zip",
      "provenanceRef": "provenance://release/2026.06.0/build",
      "provenanceDigest": "sha256:..."
    }
  ],
  "resourceUsage": {
    "wallTimeSeconds": 510,
    "peakMemoryBytes": 2147483648
  },
  "cleanupState": "complete",
  "approvalRef": null,
  "workspaceBaseRevision": null,
  "workspaceChangePolicyDigest": null,
  "patchDigest": null,
  "changedFiles": [],
  "aiDiagnosisAllowed": true
}
```

The runner streams bounded, ordered log chunks with sequence numbers and
resumable cursors. Full logs are stored as tenant-scoped artifacts only when
policy allows it. Origin domain context keeps summaries and immutable
references, not unbounded stdout or stderr.

Artifact names and paths are untrusted. The runner must enforce canonical-root
and no-follow extraction, reject traversal and special files, apply count and
byte limits, and compute the authoritative digest after bytes cross the sandbox
trust boundary.

For an agent task, `changedFiles` is the canonical manifest produced by the
trusted post-export diff, not a list supplied by the agent. The result is
accepted only when the base commit, patch digest, and workspace-change policy
digest match the lease. For a build requiring provenance, command success is
not sufficient: failure to generate or authenticate the required in-toto/SLSA
statement fails the attempt before any publish action can consume its artifacts.

If command outcome is unknown, the runner sends `status: "unknown"` with the
backend operation ID and diagnostic reference instead of fabricating a
failure. A later reconciliation report uses the same attempt and fencing token
unless the control plane has already fenced it.

## Security Requirements

- Runners authenticate to `controller-rs` with tenant-scoped credentials.
- Runner and backend control-plane traffic is encrypted and mutually
  authenticated where it crosses a host boundary.
- Execution leases are short-lived, renewable, scoped to one attempt, and
  protected by a monotonically increasing fencing token.
- A durable local watchdog stops new work on disconnect, locally fences work at
  lease expiry, revokes credential handles, and cleans tagged backend resources;
  backend-native expiry protects against runner-host failure where available.
- Runners never see workflow start events unless they are explicitly deployed
  as trusted orchestrators in a non-SaaS topology.
- Runners receive bounded execution payloads, not complete workflow
  definitions or agent sessions.
- Server-side policy decides runner pools, execution profiles, minimum
  isolation boundaries, approved backend compatibility records, immutable
  templates or images, capabilities, command templates, resources, networks,
  host exposures, mounts, workspace modes, session scopes, model provider
  scopes, data boundaries, artifacts, and credentials.
- Required backend controls and the effective rendered backend configuration
  are verified before execution; missing controls fail closed. Backend
  self-report alone cannot upgrade its trusted capability record.
- Temporary capacity shortage remains in a bounded fair queue with `retryAfter`
  and jittered claim backoff; it does not create an execution attempt or consume
  an origin retry.
- SaaS model credentials must not be sent to tenant-side runners.
- Tenant-private source code, local files, and private command logs should use
  tenant-approved model providers unless tenant policy explicitly allows SaaS
  model processing.
- Leases contain logical credential references or opaque redemption handles,
  never raw credential values.
- Immutable skill packages and other external inputs are downloaded and
  verified by trusted runner code before sandbox creation, then mounted
  read-only. Sandbox code receives no artifact-store credential or package
  download authority.
- Prefer backend-side credential injection and short-lived workload identity.
  Raw secret fallback cannot use a shared or checkpointed execution session.
- Raw tokens are forbidden in environment variables, argv, process titles,
  shell history, and persistent files. Use an attempt-bound local credential
  broker or an attempt-unique read-only `tmpfs` file when the process must receive a
  token; environment variables may carry only non-secret endpoint or path
  references.
- Secrets are task-scoped and never included in workflow context, logs,
  artifacts, snapshots, or AI prompts.
- Sandboxed model access uses a runner-owned, peer/attempt-bound local broker
  with no reusable bearer visible to the worker or generated payload. Separate
  process identities/namespaces and descriptor controls prevent generated code
  from stealing the worker's model capability; broker-side policy enforces
  model, budget, rate, cancellation, and expiry.
- TLS interception uses an operator-owned immutable trust-bundle digest and
  approved runtime adapters. Workflow code cannot add a CA or disable
  certificate verification.
- AI repair runs only in approved runner profiles and cannot publish, sign, or
  receive push credentials. A trusted diff enforces the server-owned protected
  path policy before a fixed branch or pull-request action can consume a patch.
- Publish and signing use fixed actions over immutable artifacts and require
  verified provenance, digest-bound human approval, and task-scoped isolation.
  Light-workflow dispatches typed `publish` and `sign` requests to a dedicated
  release-action service; branch and pull-request requests use a separate
  repository-action service. These credential-owning services receive exact
  immutable bindings and an idempotency key, while agents, sandboxes, runners,
  and workflow context receive no platform or signing credential.
- Human approval waiting occurs only in the origin service, `light-workflow` or
  `light-agent`; no action lease, model channel, action credential, or
  secret-bearing task environment remains active. An eligible non-secret
  session workspace may use a separate bounded hold/checkpoint, and approval
  creates a fresh common attempt and fencing token.
- Origin session close, revocation, or expiry creates a durable common cleanup
  request and promptly reclaims its physical sandbox; backend TTL is a
  last-resort bound rather than routine cleanup.
- Required build provenance is constructed and authenticated by trusted runner
  or control-plane code. Provenance signing material is inaccessible to
  tenant-controlled build steps.
- Tenant-authored jobs cannot mount host container-engine sockets.
- CPU, memory, disk, process, time, network, output, artifact, and concurrency
  limits are backend-enforced. A host-integrated backend that cannot prove a
  required limit cannot claim the task.
- Runtime drift causes quarantine, attempt fencing, credential revocation,
  cancellation, and backend cleanup.
- Unknown backend outcomes are reconciled before retry.
- All task results include runner identity, attempt, fencing token, effective
  policy digest, command template ID, backend identity and operation ID,
  artifact and provenance digests, workspace-change policy and patch digests
  where applicable, cleanup state, and approval references.

## Implementation Plan

### Phase 1: Contracts And Persistence

- Create `apps/light-workflow-runner`.
- Reuse `workflow-core` models for `run.*` task payloads.
- Define strict versioned security metadata and immutable policy snapshots.
- Define local-cleanup, workspace-change, trust-bundle, credential-projection,
  capacity-queue, and provenance policy contracts.
- Define origin-neutral execution IDs, origins, and workflow-task, agent-turn,
  and agent-action subject types in the first protocol version.
- Add common scheduling request, execution attempt, execution session,
  immutable input, execution-session cleanup request, artifact, and append-only
  runtime-audit persistence. Keep workflow approval and agent approval/domain
  state under their origin services.
- Persist tenant, trigger principal, correlation ID, and policy snapshot at
  workflow start.
- Define runner registration, heartbeat, claim, renewal, cancellation,
  reconciliation, cleanup, and result APIs.
- Define the transactional identifiers-only result-ready PostgreSQL wakeup and
  authoritative startup/periodic origin catch-up query.
- Keep the existing `light-workflow` event consumer as the only workflow start
  consumer.
- Keep unsupported `run.*` tasks disabled.

### Phase 2: Lease, Attempt, And Fencing

- Add attempt numbers, short-lived renewable leases, fencing tokens, and
  compare-and-set result acceptance.
- Add execution deadlines, cancellation, unknown-outcome state, backend operation
  IDs, and reconciliation.
- Add the durable local cleanup journal, disconnected watchdog, startup
  resource scan, and backend-native expiry handling.
- Add bounded per-tenant fair capacity queues, atomic slot reservation,
  long-poll claims, and capped jittered backoff.
- Add normalized results and bounded resumable log streaming.
- Emit the result-ready wakeup in the same transaction that stores a newly
  terminal common attempt; prove lost/duplicate notification recovery through
  indexed conditional origin acceptance.
- Prove that stale or duplicate reports cannot transition workflow or agent
  domain state and that a disconnected runner cleans resources without
  control-plane reachability.

### Phase 3: Minimal Per-Task Execution

- Add server-side runner pools, execution profiles, capability matching, and
  approved command templates.
- Implement one `run.shell` template in a task-scoped sandbox.
- Start with no credentials, no irreversible external effects, deny-all
  egress, an ephemeral workspace, and hard resource limits.
- Define the `ExecutionBackend` interface, capability document, server-owned
  compatibility record, and boundary-specific conformance suite.
- Implement Cube Sandbox as the first task-scoped microVM backend, including
  idempotent prepare and execute, inspection, cancellation, and cleanup.

### Phase 4: Artifacts, Sessions, And Additional Backends

- Add safe artifact export, trusted-side hashing, immutable storage, and
  trusted-side in-toto/SLSA provenance generation and authentication.
- Add workflow-scoped sessions with single-writer enforcement and
  copy-on-write isolation for parallel branches.
- Support agent-turn task scope and explicitly bounded agent-session reuse
  without assuming equal lifetimes; origin close/revoke/expiry must still
  trigger prompt backend cleanup.
- Add an execution-session state/fence and bounded `IDLE_APPROVAL_HOLD`
  lifecycle distinct from action leases. Missing an active attempt must not
  clean a valid held session; hold expiry must not extend the fixed maximum.
- Add idempotent pause/checkpoint/hold/resume commands and retained-resource
  quota, cost, evidence, and cleanup metrics.
- Compute effective session expiry as the minimum of origin, execution policy,
  broker/grant, and backend limits. Add durable origin-driven cleanup requests
  so close/revoke/expiry fences active work and destroys the sandbox promptly.
- Make trusted runner code download, verify, safely extract, stage, and mount
  immutable skill packages and other input records before sandbox creation;
  workers only revalidate mounted content.
- Add backend-specific production-baseline validation, including Cube
  authentication, private control-plane access, restricted inbound traffic,
  and deny-by-default egress.
- Add Docker Sandboxes for approved local or managed agent execution, requiring
  clone workspace mode for untrusted code and prohibiting the host Docker
  socket.
- Add a rootless OCI backend for explicitly trusted tasks and a Kubernetes Job
  backend only for approved runtime classes and node policies.
- Permit a runner to operate inside Toolbx only as a declared
  `host-integrated` environment; do not initially require a separate Toolbx
  adapter or allow it to claim isolated tasks.
- Add immutable TLS trust-bundle projection and approved Java, Node, Python,
  OpenSSL, and OS-store adapters.
- Add brokered credential delivery, attempt-bound local metadata service,
  read-only `tmpfs` fallback, and prohibit secret-bearing checkpoints.
- Add protected runner-broker transports using a preconnected descriptor,
  peer-checked Unix-domain socket, vsock, or backend equivalent; separate the
  trusted worker/runtime from generated payload processes and prevent broker
  descriptor inheritance.
- Add periodic effective runtime snapshots.
- Compare runner-reported config with server-approved policy.
- Quarantine drifted runners, fence attempts, revoke credentials, and clean up
  backend resources.

### Phase 5: Release And AI Workflows

- Add release-runner profile.
- Execute Java and Rust release build/test tasks through the runner.
- Add ConfigProfile manifest and `event-importer` dry-run tasks.
- Add AI failure analysis and bounded repair loops.
- Add server-owned runtime-tool manifests and placement-bound tool references.
  Gateway tools intersect gateway `tools/list`; runner tools intersect
  execution policy, lease `allowedTools`, trusted manifest, and live local
  enumeration before the independently authorized sets are combined.
- Add runner-staged immutable skill packages and runner-owned model inference
  brokering with model/data-boundary/budget enforcement outside the payload.
- Add protected-path policies, trusted post-export diff validation, and fixed
  branch or pull-request creation over accepted patches.
- Export immutable artifact sets with signed provenance and rebuild releases
  from the reviewed immutable commit after AI repair.

### Phase 6: Publish And Signing

- Add fixed publish actions or a separate release service.
- Add an external signing service or fixed task-scoped signing action.
- Bind human approval to the artifact set, target, version, command template,
  policy digest, expiry, and single-use nonce.
- End the build action lease before the origin enters `WAITING_APPROVAL`;
  create a fresh numbered domain/common fixed-action attempt, lease, monotonic fencing
  token, and grants only after approval. Apply the same contract to
  `light-agent` approvals.
- Reconcile unknown outcomes before any retry or approval reuse.

## Open Questions

- Should runner registration and task claim be direct HTTP APIs, WebSocket
  messages through `controller-rs`, or both?
- Where should long-running task logs and artifacts be stored for SaaS
  deployments?
- How should the control plane attest VM-based runners that do not have a
  container image digest?
- Which backend-native TTLs and local-watchdog deadlines are required for each
  compatibility record?
- Which protected runner-local broker transport is supported first on each
  backend: preconnected descriptor, peer-checked Unix-domain socket, vsock, or
  backend-native equivalent?
- Which protected repository paths belong in the default agent policy, and how
  are repository-specific additions approved?
- Which attestor, signing identity, storage convention, and target SLSA Build
  level should release profiles use?
- Which portal or service owns immutable execution profiles, command templates,
  backend compatibility records, conformance evidence, and their approval
  lifecycle?
- Should all publish and signing operations use a separate release service, or
  should a small set of fixed runner actions be supported?
- How much of the existing `TaskExecutor` should move into shared crates so
  `light-workflow` and `light-workflow-runner` can share evaluation and result
  handling without sharing orchestration responsibilities?

## Recommendation

Create `light-workflow-runner` as a separate executable and keep
`light-workflow` as the single SaaS-owned orchestrator. The runner should be a
fenced leased execution agent, not a workflow starter or workflow definition
loader. Integrations such as Cube Sandbox, Docker Sandboxes, rootless OCI,
approved Kubernetes runtimes, dedicated VMs, and fixed external actions belong
behind `ExecutionBackend` in the runner. Toolbx is recorded as a trusted
host-integrated runner environment, not advertised as a sandbox.

This gives tenants a practical way to run workflow tasks near their own APIs,
gateways, repositories, clusters, and sandboxes while keeping workflow start
events, policy decisions, task visibility, and audit under the SaaS control
plane. Publish and signing remain fixed, approval-bound operations over
immutable artifacts rather than arbitrary commands with release credentials.

## References

- [Execution Backends And Sandbox Execution Design](../product/light-workflow/sandbox-execution.md)
- [SLSA v1.2 Build Provenance](https://slsa.dev/spec/v1.2/build-provenance)
- [SLSA v1.2 Build Requirements](https://slsa.dev/spec/v1.2/build-requirements)
- [in-toto Attestation Framework](https://github.com/in-toto/attestation/blob/main/spec/README.md)
