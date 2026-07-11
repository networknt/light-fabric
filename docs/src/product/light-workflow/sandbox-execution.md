# Execution Backends And Sandbox Execution

## Status

Proposed product design.

`light-workflow` should support multiple execution backends for tenant-authored,
automation-heavy, and developer-local workflows. The workflow engine remains
the durable orchestrator and policy authority. Effectful work is dispatched
through the leased runner boundary defined in the
[Light-Workflow Runner design](../../design/light-workflow-runner.md), and the
runner uses a capability-described `ExecutionBackend` selected by the effective
policy.

Not every backend is a security sandbox. Cube Sandbox and Docker Sandboxes use
microVM boundaries. Rootless OCI containers and ordinary Kubernetes Jobs share
a host kernel unless a stronger runtime is configured. Fedora Toolbx is a
host-integrated developer environment and explicitly is not a sandbox. The
policy model must preserve these differences instead of treating every backend
as interchangeable.

## Problem

Workflows can be created by tenants and can eventually include tasks that run
commands, scripts, containers, model calls, MCP tools, browser automation, or
release automation. Those capabilities are useful, but they are also the
highest-risk part of the workflow runtime.

The platform needs a way to say:

- whether a workflow can request effectful execution,
- where each task is allowed to run,
- which minimum isolation boundary and host-integration limits apply,
- whether sandboxed tasks may share a workspace,
- which command, image, resource, network, filesystem, artifact, and secret
  policies apply,
- how task claims and remote execution remain correct across crashes and
  retries,
- how release workflows can keep build state without exposing publish or
  signing credentials to tenant-controlled code.

## Architecture And Ownership

Use one authoritative execution path:

```text
workflow start event
  -> light-workflow
       - creates workflow and task state
       - resolves and persists the effective policy snapshot
       - owns branching, retries, cancellation, and audit
  -> controller-rs
       - authenticates runners
       - issues and renews fenced task leases
       - rejects stale task reports
  -> light-workflow-runner
       - validates the lease and effective task policy
       - invokes the selected ExecutionBackend
       - streams bounded logs and reports normalized results
  -> execution backend
       - prepares or resumes the execution environment
       - enforces its approved isolation, resource, network, workspace,
         credential, and lifecycle policy
       - executes the approved command specification
```

Component ownership is:

- `light-workflow`: Workflow state, policy snapshots, task attempts, transition
  decisions, retry decisions, cancellation state, and durable audit.
- `controller-rs`: Runner identity, admission, capabilities, lease ownership,
  lease renewal, fencing, and quarantine.
- `light-workflow-runner`: Effectful task execution, backend selection from the
  lease, backend API credentials, log streaming, artifact transfer, and result
  normalization.
- `ExecutionBackend`: Capability-described adapter for a microVM sandbox,
  shared-kernel container, Kubernetes Job, dedicated VM, host-integrated
  environment, or fixed external action.

`light-workflow` should not hold backend control-plane credentials in the SaaS
topology and should not implement separate direct protocols for Cube, Docker,
Kubernetes, or other substrates. A local installation may colocate the runner
and backend adapter with
`light-workflow`, but it must preserve the same durable attempt, lease, fencing,
policy, result, and audit contracts.

## Goals

- Keep workflow orchestration outside effectful execution environments.
- Keep tenant-authored code outside the SaaS workflow process.
- Make the effective policy server-owned, immutable, and auditable.
- Support multiple backend purposes without weakening minimum isolation.
- Support per-task and per-workflow execution lifecycles where the backend has
  those capabilities.
- Support long-running tasks without duplicate execution caused by stale locks.
- Clean up execution resources autonomously when a runner cannot reach the
  control plane.
- Queue temporary capacity shortages without busy retry loops or consuming a
  workflow retry attempt.
- Fail closed when a required backend capability or policy control is absent.
- Keep raw release and signing credentials away from arbitrary workflow code.
- Treat agent-generated workspace changes as untrusted output and validate
  them against a server-owned path policy.
- Generate verifiable build provenance for release artifacts without exposing
  attestation credentials to tenant-controlled code.

## Non-Goals

- Do not expose vendor or backend APIs directly through the workflow DSL.
- Do not let workflow metadata define raw backend network rules or backend
  credentials.
- Do not describe Toolbx or an ordinary shared-kernel container as equivalent
  to a microVM security boundary.
- Do not store security policy or execution lifecycle state in workflow context.
- Do not promise exactly-once external side effects. The runtime provides
  fenced at-least-once execution plus explicit reconciliation.
- Do not treat a fresh sandbox as sufficient authorization for publishing or
  signing.

## First Schema Surface

Use existing metadata fields first so the design can be introduced without an
immediate `workflow-core` schema break. `WorkflowDefinitionMetadata` already
has `document.metadata`, and every task has `metadata` through
`TaskDefinitionFields`.

Workflow metadata requests security requirements through an approved profile.
It does not directly select a backend credential, mutable image name, vendor,
or raw network policy:

```yaml
document:
  dsl: "1.0.3"
  namespace: release
  name: light-fabric-polyrepo-release
  version: "1.0.0"
  metadata:
    lightWorkflow:
      runner:
        runnerPool: release
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
        containerEngine:
          access: private-daemon
        network:
          protocols:
            - https
        credentials:
          delivery: proxy-injected
```

A task may request stricter isolation and an approved command template:

```yaml
do:
  - publish-github-release:
      run:
        shell:
          command: light-release-publish
          arguments:
            - "${ .artifactSetId }"
            - "${ .version }"
      metadata:
        lightWorkflow:
          runner:
            commandTemplateId: light-fabric-release-publish-v1
          security:
            sandbox:
              sessionScope: task
              reason: release-credential-isolation
            approval:
              required: true
              bindTo:
                - artifactSetDigest
                - releaseTarget
            credentials:
              - github-release-oidc
```

The command template is the authority. The command and arguments in the
workflow must match the approved template after expression resolution. The
runner rejects a mismatch rather than executing arbitrary text.

`approval.required` makes the task ineligible for runner scheduling until
`light-workflow` has persisted a matching approval. It does not instruct a
runner to claim the task and wait. The eventual lease references the already
validated approval and represents a new fixed-action attempt.

Unknown `lightWorkflow.security` fields, unsupported schema versions, invalid
types, and unapproved profile versions must fail definition validation. They
must not be ignored.

Later, a first-class field can normalize into the same internal policy object:

```yaml
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

## Policy Dimensions

Placement, isolation boundary, backend selection, routing, session scope, and
workspace reuse are separate decisions. They must not be combined into one
`mode` value.

### Isolation Boundary

`host-integrated`

The environment deliberately shares host facilities such as the user's home
directory, session services, devices, sockets, or host networking. Fedora
Toolbx belongs in this category. It is useful for trusted local development and
troubleshooting, but it is not a security boundary for tenant-authored code.

`shared-kernel-container`

The task runs in an OCI container that shares the host kernel. Rootless Docker
or Podman and a default Kubernetes container belong here. This boundary is
appropriate for trusted build and packaging tasks when capabilities, mounts,
syscalls, resources, and networking are constrained. It must not satisfy a
profile requiring a separate kernel.

`microvm`

The task runs with a separate guest kernel in a lightweight VM. Cube Sandbox
and Docker Sandboxes belong here. A microVM can satisfy untrusted-code profiles
only when workspace, network, credential, lifecycle, control-plane, and cleanup
requirements are also enforced.

`dedicated-vm`

The task runs on a separately provisioned VM dedicated to an approved tenant,
workflow, or runner pool. This can support privileged or long-running workloads,
but image provenance, teardown, attestation, and network isolation remain
required.

`external-service`

The task calls a fixed service-owned action such as publishing, signing, or
deployment. It is not a general command environment. Authorization derives from
the action contract and immutable inputs rather than from shell isolation.

The effective profile sets a minimum boundary and an `allowedHostExposure`
allowlist. An empty allowlist means the backend may expose no host facilities.
The backend's approved `hostExposure` set must be a subset of that allowlist.
Boundary names are not the complete security decision. For example, a microVM
with a writable host workspace or raw credentials may be unsuitable for a
high-risk task.

Boundary matching uses a server-owned compatibility relation, not simple enum
sorting. An approved dedicated VM may satisfy a separate-kernel requirement,
but `external-service` is comparable only to a fixed-action requirement, and a
backend cannot claim a stronger boundary merely by changing its registration.

`workloadTrust` is `trusted` or `untrusted`. Tenant-authored code, generated
code, dynamic agent tools, and content from an untrusted repository default to
`untrusted`; workflow metadata cannot mark them trusted. An untrusted workload
requires an approved compatibility record with `supportsUntrustedCode=true` in
addition to the boundary, workspace, network, credential, and lifecycle
requirements.

### Backend Taxonomy And Intended Use

| Backend | Boundary | Intended use | Untrusted tenant code |
| --- | --- | --- | --- |
| Cube Sandbox | `microvm` | Remote or clustered tenant sandbox sessions, snapshots, controlled egress | Allowed only with the Cube production baseline |
| Docker Sandboxes (`sbx`) | `microvm` | Autonomous coding agents and isolated Docker builds | Allowed with clone workspace mode and enforced policy |
| Rootless Docker or Podman container | `shared-kernel-container` | Lightweight trusted CI, tests, packaging, and tools | Not by default |
| Kubernetes Job | Declared by approved runtime class | Scalable jobs in a tenant or service cluster | Only when the selected runtime and node policy satisfy the required boundary |
| Fedora Toolbx | `host-integrated` | Trusted developer tooling and host troubleshooting | Never |
| Dedicated VM | `dedicated-vm` | Privileged, tenant-dedicated, or long-running work | Allowed when its approved profile satisfies the task requirements |
| Publisher, signer, or deployer service | `external-service` | Fixed irreversible actions over immutable inputs | No arbitrary code surface |

Toolbx usually does not need a per-task backend implementation. A trusted local
runner may itself run inside Toolbx and register a `host-integrated` backend
with execution session scope `none`. The policy must record its home, device,
D-Bus, socket, and network exposure and prevent it from claiming isolated or
secret-bearing tasks.

An ordinary Docker container and Docker Sandboxes are different backends. A
container shares the host kernel. Docker Sandboxes place an autonomous agent
inside a microVM with a private Docker daemon. No backend may mount the host
Docker socket for tenant-authored execution.

### Backend Capability Contract

Every registered backend has an operator-approved compatibility record. A
representative capability document is:

```json
{
  "backendId": "docker-sbx-local",
  "kind": "microvm",
  "implementation": "docker-sandboxes",
  "version": "approved-version",
  "isolationBoundary": "microvm",
  "supportsUntrustedCode": true,
  "workspaceModes": ["direct", "clone"],
  "hostExposure": [],
  "networkEnforcement": ["deny-by-default", "http-l7"],
  "supportedEgressProtocols": ["http", "https"],
  "credentialDelivery": ["proxy-injected"],
  "containerEngineAccess": "private-daemon",
  "lifecycle": ["inspect", "reconnect", "cancel", "destroy"],
  "sessionScopes": ["task", "workflow"]
}
```

This is an effective, profile-specific record, not an implementation-wide
claim. It is scoped to the backend implementation and version plus the approved
template, image, runtime class, node policy, workspace mode, and enforcement
configuration that make the capabilities true. Registration and leases bind
the digest of that exact record. A configuration or compatibility change
creates a new immutable record and digest.

The minimum capability vocabulary includes:

- isolation boundary and supported trust classes,
- direct host exposure such as home, devices, D-Bus, SSH agent, localhost,
  container sockets, and writable workspace mounts,
- workspace modes: direct, ephemeral, clone, copy-on-write, and workflow reuse,
- network enforcement layer and supported protocols,
- credential delivery: proxy-injected, workload identity, attempt-bound local
  broker, or task-unique read-only `tmpfs` file; environment-value delivery is
  prohibited,
- container-engine access: none, private daemon, or prohibited host daemon,
- CPU, memory, disk, process, time, output, artifact, and concurrency controls,
- lifecycle inspection, reconnect, cancellation, snapshots, log cursors,
  operation lookup, idempotency, and cleanup,
- tenant isolation, data residency, attestation, and audit support.

Runner self-report is not sufficient. Server-owned compatibility definitions
and backend conformance tests determine which capabilities are trusted. A
backend may claim only task attempts whose effective requirements are a subset
of that approved compatibility record.

The workflow DSL does not name a backend implementation. Policy resolution
selects an eligible backend from the registered runner pool and persists the
selected backend ID, implementation, version, and capability digest in the
effective task policy.

### Placement

`host`

The task runs in the trusted `light-workflow` process. Only control-plane tasks
and explicitly approved native calls can use this placement.

`runner`

The task is sent through a fenced lease to `light-workflow-runner`. The runner
executes it through an eligible backend permitted by the effective profile.

### Execution Session Scope

`none`

The runner does not create an additional execution environment. This is allowed
only when the effective profile explicitly permits the runner environment
itself as the execution boundary.

`workflow`

One backend execution session is reused by approved tasks in one workflow
instance. This supports a shared checkout, build output, and dependency cache.
The selected backend must advertise workflow-session support. The session must
never be reused across workflow instances, tenants, principals, or incompatible
policy snapshots.

`agent-session`

One backend execution session provides a bounded interactive workspace across
turns for one authenticated agent session. Reuse requires identical tenant,
host, principal, agent definition, workspace base, policy digest, runtime
adapter, backend compatibility, network/model/tool policy, and unexpired
cleanup state. Conversation history and memory remain origin-domain state and
are never recovered from the sandbox.

`task`

One fresh backend environment is created for one task attempt. This is stricter
isolation and is required for untrusted code that must not share state, raw
credential fallbacks, and high-impact operations.

Profile labels such as `per-agent-call` or `per-publish` map to `task` scope
plus an isolation class and additional policy requirements. They are not
separate lifecycle primitives.

### Workspace Reuse

Workspace reuse is independently controlled:

- `ephemeral`: Fresh workspace for one task attempt.
- `workflow`: Reused only within one workflow instance and policy snapshot.
- `agent-session`: Reused only for the same authenticated agent session,
  principal, immutable base, runtime adapter, and policy snapshot.
- `copy-on-write`: A task receives an isolated clone of an approved workspace.

Cross-tenant caches and mutable cross-workflow workspaces are out of scope.
Shared dependency caches, if added later, require content-addressing, integrity
verification, and separate poisoning controls.

## Task Routing

Host execution remains the default for control-plane tasks:

```text
ask
assert
set
switch
workflow context merge
task creation and transition
process state persistence
approved native call.agent without tools or file access
```

Runner execution is required for effectful or tenant-local task families:

```text
run.shell
run.script
run.container
browser automation
tenant-provided code
filesystem mutation outside workflow context
external MCP server processes
command-line tools
release build and package commands
agent tasks with files, tools, or private network access
```

Calls that can run in more than one location require policy-based placement:

| Task | Host placement | Runner placement |
| --- | --- | --- |
| `call.http` | Approved SaaS endpoint and host credential boundary | Tenant-private endpoint or sandbox egress boundary |
| `call.jsonrpc` | Approved SaaS endpoint | Tenant-private or backend-local endpoint |
| `call.mcp` | Approved gateway endpoint | External process or tenant-local MCP server |
| `call.agent` | Bounded model call without tools or files | Tools, files, generated code, or tenant-local data |
| `call.rule` | Default for curated local rules | Only when an approved rule profile requires isolation |

Placement depends on endpoint identity, credential source, data boundary,
required capabilities, and network policy. Egress reachability alone is not
sufficient. Destination validation remains mandatory for host-executed HTTP,
JSON-RPC, and MCP calls.

Unsupported task types and task/backend combinations must be rejected before
execution. When the task graph can be inspected statically, definition
publication should reject the workflow before any instance can partially run.
Dynamic destinations and values are validated again for each attempt.

## Effective Policy

The runtime computes a workflow policy snapshot at instance creation and a
derived effective policy for each task attempt:

```json
{
  "policySnapshotId": "019f0000-0000-7000-8000-000000000001",
  "requestedProfile": "release-sandbox",
  "effectiveProfile": "release-sandbox",
  "profileVersion": 7,
  "policyDigest": "sha256:...",
  "placement": "runner",
  "runnerPool": "release",
  "approvedTaskTypes": ["run.shell", "call.http", "call.mcp"],
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
  "networkPolicyId": "release-egress-v3",
  "networkPolicyDigest": "sha256:...",
  "trustBundleRef": "trust-bundle://enterprise-egress-v3",
  "trustBundleDigest": "sha256:...",
  "credentialPolicy": "brokered-task-scoped",
  "artifactPolicy": "release-artifacts-v2",
  "provenancePolicy": {
    "format": "slsa-provenance-v1",
    "mode": "signed",
    "policyDigest": "sha256:..."
  },
  "localCleanupPolicyDigest": "sha256:...",
  "resourcePolicy": "release-build-medium-v1"
}
```

The snapshot is stored in dedicated runtime state, not in workflow context or
task output. Audit records reference its immutable ID and digest.

Policy resolution rules are field-specific:

- Operator profile definitions provide the base allowed backend compatibility
  records, templates, commands, networks, trust bundles, mounts,
  workspace-change policies, credentials, provenance, local cleanup, limits,
  and placements.
- Service policy intersects the profiles and capabilities available in the
  deployment.
- Tenant policy further restricts the allowed set.
- Workflow metadata requests one allowed profile and version.
- Task metadata may request stricter isolation or a subset of capabilities; it
  cannot downgrade operator-derived workload trust.
- Allowlists are intersected.
- Explicit denies take precedence.
- Numeric resource and duration limits use the lowest permitted maximum.
- `task` session scope may strengthen `workflow` scope; a task cannot weaken a
  required execution environment to `none`.
- The selected backend must meet the minimum isolation boundary and every
  required capability. A host-integrated or shared-kernel backend cannot
  satisfy a microVM requirement.
- Backend and template selections must be members of the approved set. They do
  not have a meaningful "more privileged" ordering within the same boundary.
- Credential access is the intersection of profile, task, command template,
  approval, and current credential-broker policy.

Profile versions are immutable. A new operator policy creates a new version.
An in-flight workflow continues with its recorded snapshot unless an emergency
revocation explicitly invalidates it. Revocation must fence new attempts,
cancel affected active attempts where possible, revoke credentials, and record
why the snapshot was invalidated.

Profile changes that require approval must remain pending and cannot publish an
active workflow definition. Runtime approvals for irreversible tasks are
separate objects and must bind the approver, task, command template, artifact
digest, target, policy snapshot, and expiry.

## Durable Runtime State

Remote backend execution adds distributed state and requires dedicated
persistence. Do not put session IDs, leases, policy snapshots, credentials, or
backend operation IDs in `process_info_t.context_data`.

The controller/runner portion of this state is origin-neutral. A workflow task,
standalone agent turn, or agent action can use the same scheduling, execution
attempt, lease, backend, and cleanup contract. `light-workflow` and
`light-agent` keep separate domain tables and are the only services allowed to
advance their respective subjects. See
[Light-Agent Execution](../../design/light-agent-execution.md) for agent
session and turn ownership.

The initial storage model should include:

`workflow_execution_policy_t`

- workflow process and instance IDs,
- tenant, host, trigger principal, and correlation IDs,
- requested and effective profile IDs,
- profile version and policy digest,
- creation and revocation state.

`execution_session_t`

- authenticated origin and subject scope, including optional workflow or agent
  session correlation,
- backend ID, kind, implementation, version, and capability digest,
- backend environment and session IDs,
- immutable template or image ID and digest where applicable,
- workflow policy snapshot ID,
- tenant, workflow, and principal scope,
- lifecycle state/version/fence, active action/lease owner, task and lease
  deadlines,
- optional approval-hold ID/reason, hold expiry, policy/cost binding,
  pause/checkpoint state, and retained-resource evidence,
- origin idle/max expiry, policy/grant expiry, effective minimum expiry,
  backend-native expiry, last runner contact, last inspection time, cleanup
  deadline, attempt count, and cleanup state.

`execution_session_cleanup_request_t`

- request ID, authenticated origin, origin-session/subject correlation, and
  execution-session ID,
- close, revoke, expiry, policy-change, quarantine, or operator reason,
- requested, dispatched, cleaned, retryable, or operator-action state,
- attempt/fencing watermark, retry schedule, and cleanup evidence reference,
- unique active request per execution session.

`execution_input_t`

- immutable input ID, authenticated origin subject and optional attempt or
  execution session,
- kind such as context, workspace base, skill package, trust bundle, or fixed
  action input,
- content digest, size, media/package type, storage reference, provenance and
  scanner bindings, and mount/entrypoint policy,
- staging, verification, retention, and cleanup state without embedded storage
  credentials.

`execution_attempt_t`

- authenticated origin service, execution subject kind and ID, and
  monotonically increasing attempt number,
- optional workflow task or agent turn/action correlation,
- lease ID, runner ID, and fencing token,
- backend operation ID and command idempotency key,
- effective task-policy digest,
- state, heartbeat, started, deadline, and completed timestamps,
- normalized result, error classification, and reconciliation state.

`runner_scheduling_request_t`

- idempotent scheduling request ID, authenticated origin, execution subject,
  tenant fairness key, runner pool, and effective requirements digest,
- enqueue time, queue deadline, priority class, and scheduling state,
- short-lived capacity reservation ID, runner and backend slot, reservation
  expiry, and consumption state,
- cancellation, policy-revocation, and terminal admission reason.

`workflow_approval_t`

- approval request ID, tenant, workflow, orchestration task, and state,
- artifact set and provenance digests, release target and version, command
  template, policy digest, and prior-outcome reconciliation state,
- approver identity, decision, reason, creation, decision, and expiry times,
  and single-use nonce,
- consuming post-approval execution attempt ID or rejection and expiry
  transition.

Standalone agent approvals remain in agent-domain storage, but use the same
immutable binding and single-use post-approval common-attempt contract. The
runner never owns either approval table.

`workflow_artifact_t`

- immutable artifact ID, tenant, workflow, task, and attempt IDs,
- canonical name, size, media type, and trusted digest,
- storage reference, retention class, provenance statement and envelope
  digests, attestation signer identity, and approval bindings.

The runner also maintains a minimal durable local cleanup journal before it
prepares an environment or dispatches an operation. The journal contains the
lease, fencing token, backend environment and operation IDs, absolute and
monotonic deadlines, backend-native expiry, and cleanup state. It contains no
credential values or task payloads. Runner restart recovery and the local
watchdog use this journal; the SaaS database remains authoritative for workflow
state.

Security and execution audit should be append-only. Mutable status tables can
reference the latest state, but must not replace the history needed to explain
claims, retries, cancellation, policy changes, and cleanup.

### Origin Result Wakeup

The common attempt row is authoritative. The PostgreSQL transaction that
conditionally stores a newly terminal `execution_attempt_t` also emits a
versioned `execution_result_ready_v1` notification containing only attempt ID,
authenticated origin, subject kind, and correlation ID. It carries no result
bytes, tenant content, or authorization.

`light-workflow` or `light-agent` uses that notification only to wake a
reconciler, reloads and verifies the authoritative attempt, and conditionally
accepts it into its own domain transaction. Every origin must also run indexed
startup and periodic catch-up scans because notifications can be missed,
duplicated, or reordered. A push callback may be another wakeup later, but
correctness never depends on notification delivery and controller/runner code
never updates origin-domain state directly.

## Lease, Attempt, And Fencing Model

Remote backend execution is at-least-once. Exactly-once external effects cannot
be guaranteed across the backend and workflow database boundary.

Each attempt receives a short-lived lease and a monotonically increasing
fencing token. The runner must renew the lease while the backend operation is
active. Every progress, log, artifact, and completion report includes the lease
ID, attempt number, and fencing token. The control plane rejects reports from a
stale token.

Completion updates must use compare-and-set semantics against the active
attempt. A late result from an expired attempt must not overwrite a newer
attempt. Workflow transitions occur only after the accepted result and audit
records commit.

Backend prepare and execute calls must receive stable idempotency keys when the
backend supports them. When the runner loses contact after dispatch, the
attempt enters `UNKNOWN`, not immediately `FAILED`. A reconciler inspects the
backend operation before deciding whether to accept a result, resume waiting,
cancel, or create another attempt.

The DSL `idempotencyKey` is part of the task contract, but it is not by itself a
guarantee. A side-effecting command template must declare how the target system
honors that key or how the runner queries the external operation before a
retry. Tasks without such a contract must not be automatically retried after
an unknown outcome.

## Execution Session Lifecycle

For workflow-session execution:

1. `light-workflow` resolves and persists the workflow policy snapshot.
2. `light-workflow` marks the task ready and submits an idempotent scheduling
   request. It remains `PENDING_CAPACITY` without an execution attempt when no
   eligible slot is available.
3. `controller-rs` reserves an eligible runner and backend slot. The control
   plane idempotently creates the task attempt against that reservation and
   issues its lease with the task policy, attempt number, and fencing token.
4. The runner validates the lease, its own capabilities, and the approved
   command template.
5. The runner idempotently prepares, resumes, or inspects the backend execution
   session scoped to the workflow policy snapshot.
6. The runner dispatches the command with a stable backend operation ID and
   starts lease and operation heartbeats.
7. Logs are streamed with bounded chunks and resumable sequence numbers.
8. Declared artifacts are safely copied into controlled storage and hashed
   outside the sandbox trust boundary.
9. The runner reports a normalized result with the active fencing token.
10. `light-workflow` accepts the result, persists the transition, and schedules
    cleanup when the session is no longer needed.

The session identity is scoped to:

```text
tenant id and host id
workflow definition id and version
workflow process and instance id
policy snapshot id and digest
trigger principal or approved service identity
runner pool and execution backend
```

For workflow and agent sessions, effective physical-session expiry is the
earliest of the origin session idle/max expiry, execution policy, credential or
broker-grant expiry, and backend-native TTL. The runner must not extend one
clock merely because another has time remaining.

When an origin closes, revokes, or expires its logical session, the same durable
origin transaction creates an idempotent
`execution_session_cleanup_request_t`. `controller-rs` fences and cancels
active attempts, revokes grants, and dispatches cleanup. The runner destroys
the backend session and records evidence; retries survive controller and runner
restart. A backend-native TTL is the last fail-safe. Leaving a known-abandoned
sandbox alive until that independent TTL is a cleanup defect.

An action lease and a reused execution session are different resources. Ending
an action lease always removes executable authority, model/credential broker
access, and task-scoped grants. It cleans task scope, but it does not by itself
delete a compatible `workflow` or `agent-session` workspace.

Under an explicit non-secret retention policy, an origin may put the session in
`IDLE_APPROVAL_HOLD` with a durable hold ID, reason, policy digest,
`holdUntil`, retained-resource cost, and verified checkpoint/patch evidence.
The runner pauses or checkpoints where supported. The hold expires no later
than approval expiry, idle/max lifetime, policy/cost limit, grant boundary, or
backend TTL; it cannot be extended by fake action heartbeats. Zero active
attempts is not an abandonment signal while the bounded hold is valid. Close,
revocation, policy mismatch, hold expiry, or cleanup request still destroys the
session.

Do not reuse a sandbox across tenants, unrelated workflow instances, different
policy snapshots, or incompatible principals.

A workflow session is single-writer unless the backend profile explicitly
supports safe concurrent operations. Parallel workflow branches must use
separate task sandboxes or copy-on-write clones unless their workspace access
is serialized.

Cancellation fences the attempt first, then requests backend cancellation and
environment cleanup. If cleanup cannot be confirmed, the attempt remains in a
cleanup-pending state and an orphan reconciler continues inspection. A
cancelled or expired attempt can never publish a valid completion afterward.

### Disconnected Runner Watchdog

Control-plane reconciliation alone cannot clean resources inside a tenant
network that has become unreachable. Every runner therefore has an autonomous
watchdog, separate from the task worker, with these rules:

- Write and sync the local cleanup journal before creating a backend resource.
- Stop claiming and starting work as soon as the control-plane session is
  unavailable.
- A running task may continue only while its server-issued lease remains locally
  valid. A connectivity grace period cannot extend `expiresAt` or the task
  deadline.
- At lease expiry, task deadline, cancellation observed before disconnect, or
  maximum environment lifetime, locally fence the attempt, revoke local
  credential handles, cancel the operation, and destroy or quarantine the
  environment.
- Use a monotonic deadline derived when the lease is received, bounded by the
  authenticated absolute deadline, so wall-clock rollback cannot extend
  execution.
- Tag every backend resource with tenant, workflow, attempt, policy digest,
  owner runner, and expiry. Configure a backend-native TTL or lifecycle rule
  where available so cleanup still occurs if the runner host itself fails.
- On startup and periodically, scan the local journal and backend-owned tagged
  resources, retry expired cleanup with bounded backoff, and preserve minimal
  evidence for unresolved operations.
- On reconnect, report cancellation, outcome, and cleanup evidence with the
  original lease and fencing token. The control plane may accept matching
  resource cleanup evidence for a fenced attempt, but only the current token
  can transition task outcome; an expired result remains diagnostic.

Destroying an environment cannot undo an external side effect. A disconnected
fixed action or publish operation remains `UNKNOWN` and must be inspected and
reconciled before retry. The control-plane orphan reconciler, local watchdog,
and backend-native expiry are complementary layers.

## Timeouts, Resource Limits, And Admission

The policy must distinguish:

- task wall-clock deadline,
- total workflow deadline,
- backend API request timeout,
- sandbox idle timeout,
- maximum sandbox lifetime,
- lease heartbeat interval and expiry,
- cancellation grace period,
- cleanup and evidence-retention period.

Backend idle timeout is not a task wall-clock timeout. The runner and control
plane enforce the task deadline even when backend activity keeps resetting its
idle timer.

Each profile must set backend-enforced limits for:

- CPU and memory,
- writable disk and inode or file count,
- process and PID count,
- open files,
- network destinations, connections, and optional bandwidth,
- stdout, stderr, and structured result size,
- artifact count and total bytes,
- maximum concurrent tasks and sandboxes per tenant and runner pool.

Admission checks capacity and tenant quotas before a lease is issued. The
runtime must provide backpressure instead of creating unbounded pending
sandboxes. Cost and quota exhaustion are explicit non-command failure classes.

### Capacity Queueing And Backoff

Temporary saturation is a scheduling state, not a command failure. When no
eligible runner or backend slot is available, the task remains
`PENDING_CAPACITY`; no task attempt, lease, sandbox, or workflow retry is
created. `controller-rs` owns a bounded, per-tenant fair queue and atomically
reserves capacity before issuing a lease.

The authoritative workflow task remains in `light-workflow`; the controller
queue stores only the idempotent scheduling request. When capacity opens,
`controller-rs` returns a short-lived reservation token. Attempt creation and
lease issuance bind that token through an idempotent, fenced handshake so a
lost response cannot allocate two attempts or two capacity slots.

Runner claims use long polling or server push. Empty claims and transient
backend-capacity responses carry `retryAfter` and use capped exponential
backoff with jitter. Capacity release wakes only a bounded number of eligible
waiters, and repeated backend admission failures open a short circuit breaker
instead of causing a thundering herd.

Hard tenant quota, policy, or cost-limit violations return
`execution_admission_denied` and require a policy, quota, or operator change.
Temporary saturation returns `execution_capacity_deferred` and remains queued
until capacity is available, the queue deadline expires, the workflow is
cancelled, or policy is revoked. Queue time counts toward the workflow deadline
but not the task wall-clock deadline, which begins only after lease acceptance.

## Execution Backend Interface

The provider-neutral interface is named `ExecutionBackend`. It supports
capability discovery and the normalized operations needed for crash recovery:

```text
capabilities
validate effective configuration
prepare environment idempotently
inspect environment
resume or reconnect
execute task idempotently
inspect operation
stream logs from cursor
cancel operation
export artifact safely
report measured execution evidence
create and delete checkpoint
clean up environment
```

Not every operation applies to every backend. A Toolbx-backed trusted runner
may have no separate environment to create. An external signer exposes a fixed
action rather than a shell or filesystem. Optional operations are advertised
through the approved capability record; required missing operations fail
policy resolution.

Capabilities include isolation and host exposure, resource and network controls,
credential delivery, private container-engine access, snapshots, cancellation,
log cursors, operation lookup, idempotency, measured execution evidence,
attestation support, native expiry, and cleanup behavior.

Backend-specific response codes are normalized, but request IDs and raw
diagnostic references remain in restricted audit data. Every backend adapter
must pass a boundary-appropriate conformance suite before it is enabled.

## Cube Sandbox Production Baseline

Cube Sandbox defaults are not the Light platform security policy. The Cube
adapter and deployment admission must verify the following baseline:

- CubeAPI is authenticated and authorized. Authorization checks both HTTP path
  and method.
- CubeAPI, CubeMaster, Cubelet, WebUI, Redis, and database access are restricted
  to approved private networks and protected with firewall rules.
- TLS or mTLS protects control-plane traffic where it crosses a host boundary.
- Sandbox public traffic is disabled unless an approved task explicitly needs
  an inbound service, and that service requires its per-sandbox access token.
- `allow_internet_access` is false for restricted profiles.
- L3/L4 allow rules and L7 HTTP/HTTPS rules are generated from the effective
  profile, not copied from workflow metadata.
- The effective rendered network configuration is inspected after provider
  template and request merging.
- Non-HTTP traffic, DNS, SSH, registry access, and provider-internal traffic are
  considered explicitly. L7 HTTP rules do not control arbitrary TCP or UDP.
- Templates contain the required egress CA only when TLS interception is part
  of the approved profile.
- Provider, control-plane, template, and SDK versions are pinned to a tested
  compatibility set.

The adapter must not rely on a domain list while leaving default internet
access enabled. It must not allow a workflow-supplied rule to precede or weaken
operator policy during provider rule merging.

## Docker Sandboxes Backend Baseline

Docker Sandboxes (`sbx`) is a separate microVM product, not an ordinary Docker
container. Each sandbox has its own guest kernel, Docker daemon, filesystem, and
network. It is a strong candidate for autonomous coding agents and workflows
that need to build or run containers without exposing the host Docker daemon.

The approved Docker Sandboxes backend must enforce:

- clone workspace mode for untrusted or autonomous tasks; the default direct
  workspace mount is not an isolation boundary because changes are immediately
  applied to the host working tree,
- deny-by-default network policy with only approved HTTP and HTTPS destinations,
- explicit rejection of tasks requiring raw TCP, UDP, ICMP, SSH, or private
  network access that the backend cannot provide safely,
- host-side proxy injection for supported service credentials,
- no registry, SSH, or custom credential copied into the VM unless the effective
  task policy explicitly accepts that exposure,
- the sandbox's private Docker daemon; the host Docker socket is never mounted,
- immutable backend and template or kit compatibility records,
- lifecycle inspection, reconnect, cancellation, disk quotas, and explicit
  removal after the workflow retention period,
- centrally managed organization policy for managed endpoints, or a locked and
  audited local policy for approved developer-local runners.

Docker Sandboxes is initially a developer-local or managed-endpoint backend.
Before it is used as a headless service backend, the adapter must prove stable
machine-to-machine lifecycle control, runner authentication, idempotent
operation lookup, audit export, and cleanup through the same conformance suite
used by the control plane.

## Shared-Kernel Container And Kubernetes Baseline

Rootless Docker or Podman containers and default Kubernetes Jobs share a host
kernel. They are useful for high-volume trusted builds, tests, packaging, and
approved internal tools, but they do not satisfy a `microvm` or `dedicated-vm`
requirement.

The baseline requires:

- rootless execution where supported,
- no privileged containers and no host container-engine socket,
- no host PID, IPC, or network namespace,
- dropped Linux capabilities, `no-new-privileges`, a restrictive seccomp
  profile, and the applicable SELinux or AppArmor policy,
- read-only root filesystem except for declared ephemeral volumes,
- canonical allowlisted mounts with no user-controlled host paths,
- hard cgroup CPU, memory, PID, disk, time, log, and concurrency limits,
- deny-by-default network policy and tenant-scoped service identity,
- immutable image digests and image provenance verification,
- complete pod, job, container, volume, and credential cleanup.

A Kubernetes backend records its approved runtime class and node isolation.
Kata, gVisor, dedicated nodes, or another hardened runtime may qualify for a
stronger server-owned compatibility record, but the word `Kubernetes` alone
does not establish the isolation boundary.

## Toolbx And Host-Integrated Baseline

Fedora Toolbx exists to provide a convenient mutable development and host
troubleshooting environment. It exposes the user's identity, home directory,
network, devices, D-Bus, system journal, SSH agent, and other host facilities.
It must be classified as `host-integrated`, never as a sandbox.

An approved Toolbx runner profile must:

- accept only trusted operator or developer tasks,
- advertise all host integrations in its backend capability record,
- use execution session scope `none`,
- reject tenant-authored code, dynamic agent tools, and arbitrary scripts from
  untrusted sources,
- reject publish, signing, platform credential, and cross-tenant tasks,
- rely on the host user's permissions and audit identity rather than claiming a
  separate security boundary,
- remain opt-in for local workflows and never be selected as a fallback when a
  stronger backend is unavailable.

Toolbx can be useful without a dedicated per-task adapter: the trusted
`light-workflow-runner` process can run inside a Toolbx environment and register
that environment's approved host-integrated capabilities.

## Dedicated VM And External Action Baseline

A dedicated VM backend is suitable for tenant-dedicated, privileged, or
long-running workflows when the profile pins the VM image, tenant assignment,
network, bootstrap identity, resource limits, attestation, and teardown. A
pre-existing mutable VM cannot claim untrusted work solely because it is a VM.

External action backends expose fixed typed operations instead of arbitrary
commands. Publishers, signers, and deployers validate immutable inputs,
approval bindings, target identity, and idempotency keys. They remain the
preferred backend for irreversible actions and high-value credentials.

## Release Workflow Example

A Light-Fabric release workflow can use one workflow session for build work:

```text
light-fabric
portal-service
controller-rs
light-example-rs
```

The execution path remains:

```text
light-workflow
  - owns workflow, policy snapshot, attempts, approvals, and transitions
  -> controller-rs fenced lease
  -> light-workflow-runner
       - owns backend interaction and artifact transfer
  -> workflow-session sandbox
       - checks out repositories
       - runs tests and approved build commands
       - stores workflow-scoped caches and build output
       - exports declared artifacts
```

Recommended task grouping:

```text
prepare workspace          workflow-session sandbox
checkout repositories      workflow-session sandbox
run unit tests             workflow-session sandbox
build release artifacts    workflow-session sandbox
generate release notes     workflow-session sandbox or bounded host task
publish release            per-task fixed publish action
sign artifacts             external signing service or per-task fixed action
```

Do not mount a host Docker socket into tenant-authored build sandboxes. Container
image builds should use an approved rootless builder or remote build service,
with pinned builder and base-image digests.

Build and package tasks may share state within one workflow policy snapshot.
Publish and signing tasks must not execute arbitrary scripts from that mutable
workspace. They receive only immutable artifact records from controlled
storage, verify trusted-side digests, and use an operator-owned action. Runtime
approval binds the exact artifact set, target, version, command template,
policy snapshot, and expiry.

Agent-produced changes must not flow directly into a release. After a patch is
accepted and merged, release artifacts are rebuilt from the reviewed immutable
commit under a fresh build attempt and provenance record.

## Agent Workspace Change Policy

An agent-modified workspace is untrusted output even when the agent ran in a
microVM. Every write-capable agent profile references an immutable
`workspaceChangePolicyId` and digest in the effective policy and lease. The
policy defines allowed and denied path patterns, repository roots, file types,
maximum changed files and bytes, and whether file creation, deletion, rename,
mode changes, submodule changes, or binary files are allowed. Workflow metadata
can request a stricter subset but cannot weaken this policy.

The default agent-repair policy denies changes to privilege-bearing surfaces,
including:

```text
.git/** and repository hooks
.github/workflows/** and reusable CI actions
.gitlab-ci.yml, Jenkinsfile, azure-pipelines.yml, and equivalents
CODEOWNERS and repository approval policy
workflow definitions and runner or execution-policy configuration
publish, signing, deployment, and release-credential configuration
```

Repository-specific policy adds equivalent paths used by that project. An
exception requires a distinct operator-approved profile and explicit human
review; an agent cannot request the exception itself.

In-sandbox filesystem enforcement is defense in depth. The authoritative check
happens after export in a trusted runner or control-plane component by diffing
against the immutable base commit or tree in a fresh trusted checkout, without
using repository-provided hooks or mutable Git configuration. It canonicalizes
path separators, case and Unicode according to repository rules; detects
symlink, hardlink, rename, mode, submodule, and nested-repository changes; and
creates an immutable canonical patch whose digest is validated against the path
policy. A violation returns
`workspace_change_denied`, records restricted evidence, and prevents branch,
pull-request, artifact, publish, or signing actions from consuming the patch.

The agent environment never receives repository push credentials. Branch or
pull-request creation is a separate fixed action that consumes only the
immutable accepted patch, its base commit, path-policy digest, and human-review
requirements, never the mutable agent workspace.

## Trusted Input And Skill-Package Staging

External inputs are not fetched by sandbox code. Before creating the backend
environment, trusted runner code resolves the lease's immutable
`execution_input_t` records, downloads them with runner authority, verifies
kind, size, digest, signature/provenance and scan bindings where required, and
rejects unsafe archive paths, links, devices, ownership, and expansion ratios.

The runner stages only the accepted context, workspace base, trust bundle, and
skill-package bytes and mounts them read-only with `nodev`, `nosuid`, and
`noexec` unless an approved package entrypoint requires execution.
`light-agent-worker` may revalidate the mounted package manifest, but neither
the worker nor generated code receives artifact-store credentials or outbound
package-download access. Verification or staging failure prevents sandbox
creation. Staged inputs are attempt/session scoped and are removed by the same
idempotent cleanup and watchdog path as the environment.

## Secret And Credential Handling

The execution environment must never receive broad platform credentials.
Credential access is server-owned and task-scoped.

Required rules:

- Workflow metadata references only logical credential names.
- The effective task policy and command template must both allow the credential.
- Credentials are delivered through opaque, short-lived redemption handles;
  values are never included in workflow context, leases, logs, or audit.
- Prefer backend-side or egress-proxy credential injection so arbitrary code
  cannot read the raw value.
- Prefer workload identity and short-lived OIDC tokens over static release
  tokens.
- Raw credential values are forbidden in environment variables, command-line
  arguments, process titles, shell history, workflow context, and persistent
  configuration files. Environment variables may contain only non-secret
  references such as a credential socket, metadata endpoint, or mounted-file
  path.
- When the task process must obtain a token, use a workload-authenticated local
  metadata service or broker bound to the attempt, execution identity,
  audience, scope, and short expiry. It must be unreachable from other tasks,
  must not trust an unauthenticated host-wide localhost caller, and must not log
  or cache returned values.
- Sandboxed model inference uses a runner-owned broker outside the untrusted
  payload boundary. Provider keys and reusable proxy bearer tokens are not
  projected into the worker or generated-code environment.
- Prefer a runner-created preconnected descriptor,
  peer-credential-authenticated Unix-domain socket, vsock, or
  backend-equivalent local channel. A socket pathname alone is not authority:
  the broker binds peer and attempt and enforces the approved model,
  data-boundary and policy digests, token/cost budget, rate, cancellation, and
  expiry.
- Run the trusted worker/runtime and generated payload under separate
  identities and process/mount namespaces. Deny ptrace and cross-process
  `/proc` access, and prevent broker-descriptor inheritance or reconnection by
  payload children. A model adapter that requires an extractable provider key
  is ineligible for an untrusted profile.
- A read-only, memory-backed `tmpfs` file with task-unique ownership and mode
  `0400` is the fallback for tools that cannot use a broker. Mount it only for
  the consuming process or task environment and unmount and overwrite metadata
  on completion.
- The `tmpfs` raw-value fallback is allowed only in a fresh task sandbox with
  narrow egress, no untrusted command, no pause or checkpoint, and mandatory
  termination after the task.
- Secret-bearing profiles disable core dumps, restrict cross-process `/proc`
  inspection, and exclude credential mounts from snapshots, artifacts, and
  diagnostic bundles.
- Credential revocation occurs on completion, cancellation, lease expiry,
  policy revocation, or runner quarantine.
- Log redaction is defense in depth, not the primary secret boundary. Encoded or
  transformed secrets cannot be reliably redacted after exposure.

A workflow-session sandbox must not receive raw publish or signing credentials.
Snapshots and auto-pause can preserve process memory and filesystem contents;
secret-bearing task sandboxes must use kill-on-timeout and must not be resumed.

## Network Policy

Every sandbox profile defines deny-by-default egress. A release profile may
allow destinations such as:

```text
github.com
api.github.com
ghcr.io
crates.io
index.crates.io
registry.npmjs.org
approved container registries
```

The profile must also define schemes, ports, methods, paths, DNS behavior, and
whether apex and wildcard subdomains are allowed. `github.com` does not imply
`*.github.com`, and an HTTPS allow rule does not imply SSH access on port 22.

For Cube Sandbox, restricted profiles set `allow_internet_access=false`, use
explicit L3/L4 allow targets, and add L7 rules for HTTP/HTTPS method and path
control. The adapter verifies the effective provider policy rather than
assuming the requested policy was installed.

Host-executed HTTP, JSON-RPC, and MCP calls keep destination validation, redirect
restrictions, response-size limits, and service credential policy. Sandbox
placement is not a substitute for SSRF and destination validation.

### TLS Inspection Trust Bundles

TLS interception is an explicit profile capability, never an implicit side
effect of network routing. Operator policy selects an immutable
`trustBundleRef` and digest; workflow metadata cannot supply a CA or disable
certificate verification. The trust bundle contains public CA certificates
only, never interception private keys.

Prefer installing the approved bundle in the immutable template or image. When
runtime projection is required, mount it read-only and let the approved command
template select the appropriate adapter, for example:

- the OS trust store for native tools,
- an immutable Java truststore selected with JVM truststore options,
- `NODE_EXTRA_CA_CERTS` pointing to the mounted public bundle for Node.js,
- `SSL_CERT_FILE` or `REQUESTS_CA_BUNDLE` pointing to an approved merged bundle
  for Python and OpenSSL-based tools.

These variables contain non-secret paths, not credential values. The runner
verifies the effective trust-store digest from inside the environment before
execution and records it in audit and provenance. Rotation creates a new
versioned bundle and policy digest. Tasks using certificate pinning, mTLS, or a
runtime that cannot honor the bundle must use a separately approved
non-intercepting route or fail closed; they must never fall back to disabling
TLS verification.

## Artifact And Log Boundary

The sandbox filesystem and console are untrusted input. Tasks declare candidate
artifact paths, but a glob match alone does not authorize export:

```yaml
metadata:
  lightWorkflow:
    artifacts:
      - dist/*.tar.gz
      - dist/*.sha256
      - target/release/light-workflow
```

Artifact transfer must:

- resolve paths beneath a canonical workspace root,
- refuse symlinks, hardlinks outside the root, devices, sockets, and path
  traversal,
- avoid time-of-check/time-of-use races,
- enforce per-file, total-byte, and file-count limits,
- compute the authoritative digest after bytes cross the sandbox trust boundary,
- write to immutable, tenant-scoped storage,
- record media type, provenance, template digest, command template, task
  attempt, policy digest, and retention class,
- scan or validate artifacts when the artifact policy requires it.

Task output contains references, not raw large artifacts:

```json
{
  "artifacts": [
    {
      "artifactId": "019f0000-0000-7000-8000-000000000010",
      "name": "light-fabric-0.3.0-x86_64-unknown-linux-gnu.tar.gz",
      "sha256": "...",
      "size": 12450000,
      "storeUri": "artifact://...",
      "provenanceRef": "provenance://...",
      "provenanceDigest": "sha256:..."
    }
  ]
}
```

Logs use bounded, ordered chunks with sequence numbers and resumable cursors.
The runner applies output limits before transmission. The artifact store keeps
full logs only when policy allows it; workflow context keeps summaries and
references. Log access, encryption, retention, and deletion remain tenant
scoped.

### Build Provenance Attestation

For build and release profiles, trusted-side hashing is followed by automatic
provenance generation. The interchange format is an in-toto Statement v1 with
the SLSA Provenance v1 predicate (`https://slsa.dev/provenance/v1`). The
`ExecutionBackend` supplies measured execution evidence, but a trusted runner
supervisor or control-plane attestor constructs and authenticates the final
statement; tenant-controlled build steps cannot choose or rewrite its fields.

The statement binds at least:

- each exported artifact name and trusted digest as an attestation subject,
- the command-template build type, template version, resolved argument digest,
  and policy-approved external parameters,
- source repository URI, immutable commit and tree digest, input artifact
  digests, base images, and best-effort resolved dependencies,
- workflow, task, attempt, lease, and policy snapshot identities,
- runner and builder identity and version,
- backend kind, implementation, capability record, template or image digest,
  runtime class where applicable, and execution session isolation,
- resource, network, workspace-change, credential-delivery, and trust-bundle
  policy digests,
- start and completion timestamps, outcome, and completeness metadata.

The attestation is stored immutably beside the artifacts and referenced by URI
and digest. When signed provenance is required, signing occurs outside the
tenant execution environment with a short-lived workload identity or key that
tenant code cannot access. Publish and signing actions verify the attestation
signature, subject digests, builder identity, policy expectations, and approval
bindings before consuming an artifact.

Using the SLSA format does not by itself establish a SLSA Build level. A profile
may declare `provenanceMode: unsigned` for development or `signed` for release,
but the platform must be assessed against the applicable SLSA build-platform
and isolation requirements before making a level claim. Host-integrated builds
must not inherit a hosted or isolated level merely because they emitted the
same JSON shape. Provenance records claims and evidence about the build; it does
not by itself prove artifact correctness or that the execution environment was
uncompromised.

## Audit

Every sandboxed task attempt records:

- tenant, host, trigger principal, and correlation ID,
- workflow definition ID and version,
- workflow process and instance ID,
- task ID, task name, and attempt number,
- runner ID, lease ID, and fencing token,
- requested profile and immutable effective policy digest,
- backend ID, kind, implementation, version, capability digest, and request ID,
- execution session ID, immutable template or image digest, and backend
  operation ID,
- command template ID, resolved argv digest, working directory, immutable base
  commit or tree, workspace-change policy and accepted patch digests, and
  environment name allowlist,
- credential names and delivery mechanism, never values,
- requested and effective network, trust-bundle, resource, and local-cleanup
  policy digests,
- approval IDs and the artifact and target digests they authorize,
- artifact metadata, provenance statement and envelope digests, builder and
  signer identities, and verification result,
- exit status, duration, resource usage, output sizes, and log reference,
- cancellation, disconnect, watchdog action, retry, reconciliation, and cleanup
  events.

For `call: agent`, also record model provider scope, model name, prompt profile,
token budget, output schema ID, validation result, tool policy, and data
boundary, plus the canonical changed-file manifest and path-policy result.

Audit records are append-only. Workflow-visible output must not contain
backend credentials, control-plane tokens, raw injected secrets, or restricted
backend diagnostics.

## Failure And Retry Handling

Backend and runner failures map to stable workflow errors:

- hard admission, quota, or cost-policy failure: `execution_admission_denied`,
- temporary capacity shortage: `execution_capacity_deferred`,
- capacity queue deadline expired: `execution_queue_timeout`,
- policy rejection: `execution_policy_denied`,
- unsupported capability: `execution_capability_missing`,
- environment startup failure: `execution_start_failed`,
- task wall-clock timeout: `execution_timeout`,
- command non-zero exit: `command_failed`,
- agent patch violates the workspace policy: `workspace_change_denied`,
- required provenance cannot be generated or authenticated:
  `provenance_generation_failed`,
- oversized result, log, or artifact: `execution_output_too_large`,
- lease expiry: `runner_lease_expired`,
- cancellation: `execution_cancelled`,
- backend outcome not yet known: `execution_outcome_unknown`,
- cleanup not confirmed: `execution_cleanup_pending`.

Retry classification is explicit:

- Policy, validation, and unsupported-capability failures are not retried.
- `execution_capacity_deferred` stays in the fair scheduling queue and does not
  consume a command retry attempt.
- Idempotent startup may retry using the same session idempotency key.
- A non-zero command exit follows the workflow retry policy only when the
  command template declares retry safety.
- Transport loss after dispatch enters `UNKNOWN` and is reconciled before a new
  attempt.
- Timeout requests cancellation, fences the attempt, and inspects the backend
  before retry evaluation.
- External side effects require a target-system idempotency or reconciliation
  contract. Otherwise, an unknown result requires operator intervention.

The workflow process does not transition until it has accepted a result from
the current fenced attempt and committed the result, audit, artifact records,
and next-task creation atomically in the workflow database.

## Cleanup And Retention

Sandbox termination and evidence retention are separate responsibilities.

- Workflow execution sessions are cleaned after workflow completion, permanent
  failure, cancellation, maximum lifetime, or policy revocation.
- Per-task backend environments are cleaned after the task result and required
  evidence are secured.
- Backend snapshots and checkpoints are independent resources and must be
  explicitly deleted according to policy.
- Failed destroy or delete calls create cleanup-pending records for the orphan
  reconciler.
- Backend metadata tags include tenant, workflow instance, policy snapshot,
  session record, and expiry so orphan discovery does not depend only on the
  workflow database.
- The runner watchdog cleans expired local journal entries and tagged backend
  resources when control-plane contact is unavailable. Native backend expiry
  remains required where possible in case the runner host also fails.
- Retained snapshots, logs, and artifacts require encryption, tenant isolation,
  retention limits, deletion audit, and data-residency policy.
- Secret-bearing sandboxes cannot be paused, checkpointed, or retained for
  debugging.

## Definition And Runtime Approval

Definition approval and task approval solve different problems.

Definition approval covers profiles that enable command execution, external
MCP processes, broad network access, mutable mounts, credential access, or
high-cost resource classes. The approval produces an immutable workflow
definition and profile version.

Runtime approval covers a specific irreversible action. A publish or signing
approval includes:

```text
tenant and workflow instance
task and attempt
command template
artifact set and trusted digest
release target and version
effective policy digest
approver and approval time
expiry and single-use nonce
```

Changing any bound value invalidates the approval. A new attempt after an
unknown outcome requires reconciliation and may require a new approval; it
must not silently reuse the prior approval.

### Approval Is An Orchestration State

Waiting for a person is owned by the authenticated origin service—
`light-workflow` for workflow tasks or `light-agent` for standalone agent
actions—never by a runner task:

```text
build or stage task completes
  -> artifacts and provenance commit
  -> action lease ends and task credentials/model channel are revoked
  -> task sandbox cleans, or eligible session workspace enters bounded hold
  -> origin persists WAITING_APPROVAL
  -> approval is granted and bindings are revalidated
  -> origin creates a new numbered domain and common execution attempt
  -> controller-rs issues a fresh lease, fencing token, and scoped grants
```

When approval is known before dispatch, the origin records the bound action
intent but creates no common execution attempt until approval. If a running
agent/runtime discovers the boundary, it returns a known
`approval_required` terminal result; its lease and grants end and its sandbox
is cleaned or explicitly checkpointed under bounded non-secret policy. Approval
never reactivates that attempt.

The origin transaction that enters `WAITING_APPROVAL` persists exactly one
session disposition—cleanup or a policy-valid bounded hold. If common session
state is later moved to another database, use an idempotent transactional
outbox. A session reaper must not infer abandonment from the ended action lease
before that disposition is durable.

The runner must not poll for approval, hold or renew a task lease, or keep a
secret-bearing environment alive while the workflow waits. A non-secret
workflow or agent session may be paused/checkpointed or retained only through
the separate bounded hold contract when explicit cost, maximum-lifetime, and
retention policy allows it. Important uncommitted work should also be exported
as an approved immutable patch/checkpoint so correctness does not depend only
on the live sandbox. The preferred release path exports immutable artifacts
and provenance, then cleans the build environment.

Approval rejection or expiry transitions the orchestration state without
dispatching a runner task. Approval grants authorize only the new fixed-action
attempt and are rechecked against its operation, arguments, artifact,
provenance, target, policy, command template, expiry, and nonce. The approval is
consumed once by the new common attempt, which has a monotonic fencing token.
The prior attempt, lease, backend handle, and grants remain immutable and are
never returned to the build or agent environment. A held workspace may be
reused by the fresh action only after principal/base/runtime/policy/expiry and
cleanup-state revalidation; otherwise restore only a verified policy-permitted
checkpoint/patch into a fresh environment.

## Implementation Plan

### Phase 1: Contracts And Persistence

1. Define versioned security metadata and strict validation.
2. Define field-specific policy resolution and immutable policy snapshots.
3. Add dedicated policy, execution session, session-cleanup request, immutable
   input, common attempt, artifact, approval, and audit persistence.
4. Define workspace-change, trust-bundle, credential-projection, local-cleanup,
   capacity-queue, and provenance policy contracts.
5. Persist tenant, trigger principal, correlation ID, and policy snapshot on
   workflow start.
6. Define the identifiers-only transactional result-ready PostgreSQL wakeup
   plus indexed startup/periodic origin catch-up.
7. Keep unsupported `run.*` task types disabled.

### Phase 2: Runner Lease And Fencing

1. Align with the `light-workflow-runner` registration and lease protocol.
2. Add attempt numbers, lease renewal, fencing tokens, compare-and-set result
   acceptance, cancellation, and reconciliation.
3. Add normalized result and resumable log contracts.
4. Store terminal common results and emit origin wakeups in one transaction;
   prove correctness when notifications are missed, duplicated, or reordered.
5. Add the durable runner cleanup journal, autonomous watchdog, startup scan,
   and backend-native expiry contract.
6. Add bounded per-tenant fair capacity queues, atomic slot reservation, and
   jittered claim backoff.
7. Prove that stale or duplicate reports cannot transition a workflow or agent
   domain object.

### Phase 3: Minimal Per-Task MicroVM Backend

1. Define the `ExecutionBackend` trait, capability vocabulary, compatibility
   records, and boundary-appropriate conformance tests.
2. Implement Cube Sandbox capability discovery and production-baseline checks.
3. Support one approved `run.shell` command template in a per-task sandbox.
4. Start with no credentials, no external side effects, deny-all egress, and
   hard resource limits.
5. Add cancellation, orphan cleanup, and backend operation reconciliation.

### Phase 4: Additional Backends, Artifacts, And Sessions

1. Add safe artifact extraction, trusted-side hashing, immutable storage, and
   trusted-side in-toto/SLSA provenance generation.
2. Add Docker Sandboxes for autonomous developer-local agents, requiring clone
   workspace mode and an approved policy.
3. Add rootless OCI container execution for trusted build and packaging
   profiles.
4. Allow trusted runners hosted in Toolbx to register only host-integrated,
   no-sandbox capabilities; no per-task Toolbx adapter is required initially.
5. Add Kubernetes Jobs only with an approved runtime-class compatibility
   record.
6. Add workflow-session reuse with single-writer enforcement where supported.
7. Add bounded agent-session reuse with effective minimum expiry and durable
   origin close/revoke/expiry cleanup requests.
8. Add session state/version/fencing plus idempotent hold/pause/checkpoint,
   resume, and cleanup operations. `IDLE_APPROVAL_HOLD` is separate from an
   action lease, bounded by effective expiry and cost policy, and never carries
   model or credential authority.
9. Add trusted runner-side download, verification, safe extraction, staging,
   read-only mounting, and cleanup for immutable skill packages and inputs.
10. Add copy-on-write isolation for parallel branches and agent repair tasks.
11. Add checkpoint lifecycle and deletion without allowing secret-bearing
   snapshots.
12. Add server-owned agent workspace-change policies and trusted post-export
   diff validation before branch or pull-request creation.

### Phase 5: Network And Credential Profiles

1. Add deny-by-default L3/L4 and L7 policy generation and verification.
2. Add immutable TLS trust-bundle profiles and language-runtime adapters.
3. Add brokered credential handles, authenticated local metadata endpoints,
   read-only `tmpfs` fallback, and backend-side credential injection.
4. Add protected runner-owned model-broker transports, separate worker/payload
   identities, descriptor isolation, and broker-side model and budget policy.
5. Add short-lived workload identity and revocation.
6. Add runtime approval records bound to immutable inputs.

### Phase 6: Release, Publish, And Signing

1. Add release build/test/package workflows using workflow sessions.
2. Export immutable artifact sets with provenance.
3. Add fixed publish actions or a dedicated release service.
4. Add external signing or a fixed per-task signing action.
5. Require digest-bound human approval and complete unknown-outcome
   reconciliation before retry.
6. Persist approval waiting only in the origin service; end any current action
   lease before waiting, use only the separate bounded session-hold contract
   where allowed, and issue a new numbered domain/common fixed-action attempt,
   lease, fencing token, and grants after approval.

## Acceptance And Failure-Injection Tests

The feature is not ready for tenant workloads until tests prove:

- a command lasting beyond the old task-lock interval is not claimed twice,
- an expired runner cannot report success after a new attempt starts,
- a crash after backend dispatch but before database commit is reconciled,
- a terminal result committed while its origin listener is offline is found by
  indexed catch-up and accepted once; duplicate or reordered wakeups do not
  duplicate the domain transition,
- repeated create and execute requests do not create duplicate operations,
- a disconnected runner stops new work, locally fences execution at lease
  expiry, and cleans the environment without control-plane reachability,
- runner restart replays the cleanup journal, and backend-native expiry cleans
  resources when the runner host does not restart,
- temporary saturation remains queued with bounded jittered backoff and does
  not create attempts, sandboxes, or a claim storm,
- a `host-integrated` or `shared-kernel-container` backend cannot claim a task
  requiring `microvm`,
- a Toolbx runner cannot claim tenant-authored, isolated, or credential-bearing
  tasks,
- Docker Sandboxes direct workspace mode is rejected for untrusted tasks and
  clone mode preserves the host repository boundary,
- no tenant-authored backend receives a host container-engine socket,
- a Kubernetes Job cannot claim a stronger boundary than its approved runtime
  class and node policy provide,
- cancellation fences results and eventually removes the sandbox,
- origin session close, revocation, and expiry fence active work and reclaim a
  reused sandbox without waiting for backend-native TTL, even across controller
  or runner restart,
- policy revocation stops new attempts and revokes credentials,
- denied network destinations remain denied for HTTP, non-HTTP, DNS, and direct
  IP access,
- backend control-plane requests require authorization,
- output, log, artifact, process, disk, and time limits are enforced,
- symlink and path-traversal artifact exports fail,
- agent changes to CI/CD, workflow, approval, release, signing, deployment, or
  repository-policy paths are rejected before PR or branch creation,
- case, Unicode, symlink, rename, mode, submodule, and nested-repository tricks
  cannot bypass the workspace-change policy,
- raw credentials do not appear in process memory snapshots, logs, task output,
  artifacts, or workflow context for the preferred credential path,
- raw tokens are never placed in environment variables or argv, and metadata
  service and `tmpfs` projections are isolated to the leased task,
- generated payload code cannot inspect or inherit the worker's model-broker
  channel, obtain a provider/proxy bearer, impersonate another attempt, choose
  an unauthorized model, or exceed broker-enforced budget,
- skill-package digest/signature mismatch, unsafe archive content, or staging
  failure prevents sandbox creation, and sandbox code cannot access the
  artifact store,
- the effective TLS trust-bundle digest is verified without allowing workflow
  code to add a CA or disable certificate validation,
- snapshots and failed sandbox creations are found and cleaned by the orphan
  reconciler,
- required provenance binds the trusted artifact digest, source and input
  digests, command template, builder and backend identity, and policy digest;
  tampering or an unexpected signer blocks publish,
- no action lease, model channel, action credential, or secret-bearing task
  environment remains active while a workflow or agent waits for approval;
  an eligible non-secret session survives only through a distinct bounded
  hold/checkpoint, and approval creates a fresh domain/common fixed-action
  attempt, lease, and fencing token without reopening the prior attempt,
- releasing an action lease does not prematurely clean a valid approval-held
  session, while hold/session expiry and close/revocation always trigger
  cleanup; resume cannot extend the fixed maximum or restore unverified state,
- publish approval fails if the artifact digest, target, policy, command, or
  expiry changes.

## Open Decisions

- Whether approved execution profiles live only in service configuration or are
  also portal-managed immutable records.
- Whether artifact metadata lives in portal tables while bytes live in object
  storage, or whether another artifact service owns both.
- Whether all publish and signing operations use a separate release service or
  whether a limited set of fixed runner actions is supported.
- Which Cube and Docker Sandboxes versions and capabilities form the first
  supported compatibility sets.
- Which hardened Kubernetes runtimes and dedicated-VM attestation mechanisms
  should be approved for untrusted workloads.
- Whether Toolbx support should remain an operational runner profile or later
  gain explicit local-runner lifecycle helpers.
- Which watchdog deadlines, backend-native TTL mechanisms, and cleanup evidence
  are required for each backend compatibility record.
- Which protected runner-local broker transport is supported first for each
  backend: preconnected descriptor, peer-checked Unix-domain socket, vsock, or
  backend-native equivalent.
- Which repository paths are protected by the default agent policy and how
  repository-specific additions are approved.
- Which provenance signer, transparency or timestamp mechanism, storage
  convention, and target SLSA Build level release profiles require.
- How emergency policy revocation balances immediate termination against
  preserving forensic evidence.
- Whether a first-class `security` field should be added to `workflow-core`
  after the metadata-based contract proves stable.

## References

- [Light-Workflow Runner Design](../../design/light-workflow-runner.md)
- [Cube Sandbox Introduction](https://cubesandbox.com/guide/introduction.html)
- [Cube Sandbox Lifecycle](https://cubesandbox.com/guide/lifecycle.html)
- [Cube Snapshot, Rollback, And Clone](https://cubesandbox.com/guide/snapshot-rollback-clone.html)
- [Cube Egress Network Policy](https://cubesandbox.com/guide/network-policy.html)
- [Cube Security Proxy](https://cubesandbox.com/guide/security-proxy)
- [Cube Restrict Public Access](https://cubesandbox.com/guide/restrict-public-access.html)
- [Cube Authentication](https://cubesandbox.com/guide/authentication)
- [Cube Network Hardening](https://cubesandbox.com/guide/network-hardening.html)
- [Docker Sandboxes](https://docs.docker.com/ai/sandboxes/)
- [Docker Sandboxes Architecture](https://docs.docker.com/ai/sandboxes/architecture/)
- [Docker Sandboxes Security Model](https://docs.docker.com/ai/sandboxes/security/)
- [Docker Sandboxes Default Security Posture](https://docs.docker.com/ai/sandboxes/security/defaults/)
- [Docker Sandboxes Credentials](https://docs.docker.com/ai/sandboxes/security/credentials/)
- [Fedora Silverblue Toolbx](https://docs.fedoraproject.org/en-US/fedora-silverblue/toolbox/)
- [Toolbx](https://containertoolbx.org/)
- [SLSA v1.2 Build Provenance](https://slsa.dev/spec/v1.2/build-provenance)
- [SLSA v1.2 Build Requirements](https://slsa.dev/spec/v1.2/build-requirements)
- [in-toto Attestation Framework](https://github.com/in-toto/attestation/blob/main/spec/README.md)
