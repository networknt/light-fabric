# Light-Workflow Runner

## Status

Proposed design.

`light-workflow-runner` is a tenant-side execution agent for workflow tasks
that must run near tenant systems, tenant repositories, private tools, local
gateways, sidecars, or sandboxed release workspaces. It is not a second
workflow engine and it must not consume workflow start events directly.

The SaaS-owned `light-workflow` instance remains the authoritative
orchestrator. It consumes workflow start events, creates workflow instances,
persists task state, resolves workflow definitions, applies policy, and owns
audit history. Tenant runners register with `controller-rs`, receive
server-issued task leases, execute only the leased task, and report normalized
results back to the control plane.

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
Domain Event
  |
  | consumed by SaaS control plane only
  v
light-workflow
  |
  | workflow instance, tasks, policy, audit
  v
controller-rs
  |
  | registration, leases, heartbeat, audit
  v
light-workflow-runner
  |
  | local command, sandbox, MCP, deploy, release tools
  v
Tenant Runtime Environment
```

The split is:

- `light-workflow`: Authoritative orchestrator. It sees workflow start events,
  loads workflow definitions, creates tasks, computes effective task policy,
  and records state.
- `controller-rs`: Runtime control plane. It authenticates runners, records
  runner capabilities, issues task leases, receives heartbeats, audits runtime
  config, and quarantines mismatched runners.
- `light-workflow-runner`: Tenant-side execution agent. It claims only leased
  work, executes the assigned task in the approved environment, streams logs,
  and reports normalized results.
- Sandbox or VM: Optional execution substrate used by the runner for high-risk
  tasks such as release builds, AI repair, scripts, and publishing.

The runner can run beside tenant APIs, gateways, sidecars, and deployers. It
may also run in a prepared release VM or sandbox with approved tools and
repository workspaces.

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
    "docker",
    "event-importer"
  ],
  "imageDigest": "sha256:...",
  "configHash": "sha256:...",
  "commandAllowlistHash": "sha256:...",
  "workspacePolicy": "release-workspace-v1",
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

## Task Lease Model

The task lease is the enforcement object. The runner should execute a task only
when it has a valid lease issued by the control plane.

Lease example:

```json
{
  "leaseId": "01970f5d-0000-7000-8000-000000000001",
  "tenantId": "tenant-a",
  "hostId": "host-a",
  "runnerId": "release-runner-01",
  "wfInstanceId": "release-2026.06.0",
  "taskId": "build-java-products",
  "taskType": "run.shell",
  "runnerPool": "release",
  "executionProfile": "release-sandbox",
  "capabilities": ["git", "maven"],
  "commandTemplateId": "light-fabric-release-build",
  "expiresAt": "2026-06-08T19:30:00Z",
  "nonce": "single-use-random-value"
}
```

Server-side validation must check:

- runner session is active,
- runner is not quarantined,
- tenant and host match,
- task runner pool matches registered pool,
- task execution profile is allowed,
- required capabilities are a subset of effective runner capabilities,
- command template is approved,
- lease is not expired,
- lease has not already been used.

The runner reports task start, logs, progress, and final result using the lease.
The control plane rejects reports that do not match the active lease.

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
task lease. Use this mode when the agent needs access to tenant-local state or
effectful tools:

- checked-out repositories,
- command output plus working directory inspection,
- private tenant network access,
- local MCP servers,
- sandbox tools,
- AI repair of source code,
- test reruns,
- branch or pull-request creation.

The main `light-workflow` instance still creates the task and records the
result. `controller-rs` issues a lease only to a runner whose effective
capabilities, runner pool, execution profile, command allowlist, workspace
policy, and audit state match the task requirements.

Runner agent lease example:

```json
{
  "taskType": "call.agent",
  "agentPlacement": "runner",
  "runnerPool": "release",
  "executionProfile": "release-sandbox",
  "sandboxMode": "per-agent-call",
  "sandboxProvider": "cubesandbox",
  "modelProviderScope": "tenant",
  "modelProviderRef": "tenant-openai-eastus",
  "credentialRef": "runner-secret://llm-provider",
  "dataBoundary": "tenant-network",
  "allowedTools": ["git", "maven", "cargo"],
  "workspaceAccess": "copy-on-write-release-workspace",
  "networkPolicy": "release-egress",
  "secretPolicy": "none",
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

### Runner Agent Sandbox Isolation

The runner itself is a tenant-side execution agent. For stronger isolation, the
runner can launch the agent task inside a separate sandbox such as Cube
Sandbox, a VM, or a Kubernetes Job. This should be a tenant-selectable policy
because the runner is deployed in the tenant namespace, but the effective
choice must still be recorded and enforced by the control plane.

Recommended isolation levels:

| Isolation Level | Use Case | Default Policy |
| --- | --- | --- |
| no sandbox | bounded model call with no tools, no file access, and no private network mutation | allowed for low-risk tasks |
| workflow-session sandbox | release build/test/diagnosis that needs the same checkout and cache across steps | useful for release workflows |
| per-agent-call sandbox | AI repair, arbitrary code inspection, generated patches, dynamic tools, or untrusted scripts | preferred for high-risk agent tasks |
| per-publish sandbox | signing, publish tokens, artifact upload, and final tag push | required for high-value secrets |

For a release workflow, the runner should usually orchestrate a separate
per-agent-call sandbox for AI repair. The runner injects only the leased
workspace, approved tools, network policy, and task-scoped secrets. It collects
logs, artifacts, patches, and structured output, then destroys or freezes the
sandbox according to retention policy.

This creates a layered boundary:

```text
SaaS light-workflow
  -> controller-rs task lease
  -> tenant light-workflow-runner
  -> per-agent sandbox
  -> model, tools, files, network
```

Tenants may choose Cube Sandbox, VM isolation, Kubernetes Job isolation, or no
sandbox for allowed profiles. Runner registration must advertise supported
sandbox providers and modes. If a task requires `per-agent-call` isolation and
the runner cannot provide it, `controller-rs` must not issue the lease.

Local runner config can select among tenant-approved profiles, but it cannot
weaken a task requirement. The lease contains the final effective
`sandboxMode`, `sandboxProvider`, workspace, network, tool, and secret policy.
Heartbeat and audit snapshots should prove the runner is still operating under
that profile.

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
The runner should resolve model credentials from tenant-controlled secret
stores or tenant-approved local provider configuration. SaaS model credentials
must not be sent to tenant runners.

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
  "modelProviderRef": "tenant-openai-eastus",
  "credentialRef": "runner-secret://llm-provider",
  "dataBoundary": "tenant-network"
}
```

Recommended placement rule:

```text
bounded reasoning over workflow context -> native call: agent in light-workflow
agent needs files, tools, or private network -> leased runner agent
interactive session or dynamic tool loop -> containerized light-agent service
```

For release workflows, use native `call: agent` to summarize and classify a
failed command. Use a runner agent for repo inspection, patch generation, test
rerun, and pull-request creation. Human approval remains required before
publish, signing, or final tag creation.

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
        executionProfile: release-sandbox
        capabilities:
          - git
          - maven
          - docker
```

Task-level example:

```yaml
do:
  - build-java:
      run:
        shell:
          command: ./release.sh
          arguments:
            - "${ .release.version }"
      metadata:
        lightWorkflow:
          runner:
            runnerPool: release
            commandTemplateId: light-fabric-release-build
          security:
            sandbox:
              mode: workflow-session
```

Runtime policy resolution:

1. Workflow definition requests a runner profile.
2. Task metadata can request stricter handling.
3. Tenant policy sets the maximum tenant privilege.
4. SaaS service policy sets global allowed runner types.
5. Operator-approved profile definitions set allowed commands, networks,
   images, mounts, sandbox modes, sandbox providers, model provider scopes,
   data boundaries, and secrets.
6. `controller-rs` validates actual registered runner state.
7. The task lease contains the final allowed execution scope.

A task may request stricter isolation than the workflow, but it must not weaken
the effective policy.

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
- supported sandbox modes and providers,
- sandbox provider and template,
- allowed model provider scopes,
- network zone,
- secret policy,
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
  "activeLeases": 1,
  "timestamp": "2026-06-08T19:00:00Z"
}
```

If a hash changes unexpectedly, the controller marks the runner suspicious and
stops issuing new leases.

### Periodic Deep Audit

Periodically, `controller-rs` should request an effective runtime snapshot from
the runner and compare it with the approved policy. For high-risk runners, the
snapshot should include command allowlist, sandbox template, mount list, network
policy, and secret bindings.

On mismatch:

1. Mark runner as `quarantined`.
2. Revoke active claim credentials.
3. Stop issuing new leases.
4. Emit a runtime audit event.
5. Create an operator task if active work may be affected.

Audit is not the only enforcement mechanism. It detects drift after admission.
The task lease remains the primary runtime authorization boundary.

## Release Runner Mode

A release runner is a specialized `light-workflow-runner` profile.

It can run in:

- a prepared VM,
- a Cube Sandbox session,
- a Kubernetes Job,
- a controlled bare-metal release host.

Recommended default for release workflows:

- one workflow-session sandbox or VM workspace for checkout, build, test, and
  package steps,
- per-agent-call sandbox isolation for AI repair, source inspection, generated
  patches, and test reruns driven by an agent,
- per-task sandbox isolation for publishing, signing, and tasks with release
  secrets,
- clean checkout inside the runner rather than writable host repository mounts,
- artifact export through controlled storage,
- AI repair limited to sandbox workspace changes or branch/PR creation.

Writable host mounts should be avoided for AI repair and release commands. If
host repositories must be mapped, default to read-only mounts and copy the repo
into a runner-owned working directory before mutation.

## Runner API

The first runner API can be small.

```text
POST /runner/register
POST /runner/heartbeat
POST /runner/claim
POST /runner/task/{leaseId}/started
POST /runner/task/{leaseId}/log
POST /runner/task/{leaseId}/complete
POST /runner/task/{leaseId}/fail
POST /runner/audit-snapshot
POST /runner/drain
```

`controller-rs` can expose these APIs directly or mediate them over its
existing persistent connection model. For private tenant networks, outbound
runner registration and polling is preferable to inbound SaaS calls into the
tenant environment.

The claim response should include only the task payload needed for execution,
not the full workflow definition.

## Command Result Contract

Runner results should use a normalized command result so `light-workflow`,
human tasks, AI diagnosis, and audit do not depend on raw console parsing.

```json
{
  "leaseId": "01970f5d-0000-7000-8000-000000000001",
  "taskId": "build-java-products",
  "runnerId": "release-runner-01",
  "attempt": 1,
  "status": "failed",
  "exitCode": 1,
  "startedAt": "2026-06-08T19:10:00Z",
  "completedAt": "2026-06-08T19:18:30Z",
  "summary": "Maven test failure in db-provider",
  "stdoutRef": "artifact://release/2026.06.0/build/stdout.log",
  "stderrRef": "artifact://release/2026.06.0/build/stderr.log",
  "artifactRefs": [
    "artifact://release/2026.06.0/build/surefire-reports.zip"
  ],
  "changedFiles": [],
  "aiDiagnosisAllowed": true
}
```

The runner should stream logs in chunks and store full logs as artifacts.
Workflow context should keep summaries and artifact references, not unbounded
stdout or stderr.

## Security Requirements

- Runners authenticate to `controller-rs` with tenant-scoped credentials.
- Task leases are short-lived, single-use, and scoped to one task.
- Runners never see workflow start events unless they are explicitly deployed
  as trusted orchestrators in a non-SaaS topology.
- Runners receive task payloads, not complete workflow definitions.
- Server-side policy decides runner pools, execution profiles, capabilities,
  commands, networks, mounts, sandbox modes, model provider scopes, data
  boundaries, and secrets.
- SaaS model credentials must not be sent to tenant-side runners.
- Tenant-private source code, local files, and private command logs should use
  tenant-approved model providers unless tenant policy explicitly allows SaaS
  model processing.
- Secrets are task-scoped and never included in logs or AI prompts.
- AI repair runs only in approved runner profiles and cannot publish or sign.
- Publish and signing tasks require human approval and per-task isolation.
- Runtime drift causes quarantine and lease revocation.
- All task results include runner identity, effective policy version, command
  template ID, artifact references, and approval references.

## Implementation Plan

### Phase 1: Split Runner Boundary

- Create `apps/light-workflow-runner`.
- Reuse `workflow-core` models for `run.*` task payloads.
- Define runner registration, heartbeat, claim, and result APIs.
- Add server-side runner pools and execution profiles.
- Keep the existing `light-workflow` event consumer as the only workflow start
  consumer.

### Phase 2: Leased Run Task Execution

- Implement `run.shell` execution in the runner.
- Add command template allowlists.
- Add normalized command result output.
- Add log streaming and artifact references.
- Route eligible `run.shell` tasks from `light-workflow` to registered
  runners through `controller-rs`.

### Phase 3: Sandbox and Workspace Policy

- Add workflow-session and per-task sandbox modes.
- Support release VM or Cube Sandbox runner profiles.
- Add workspace mount and checkout policies.
- Add network and secret policy enforcement.
- Add runtime config hash reporting.

### Phase 4: Audit and Quarantine

- Add periodic effective runtime snapshots.
- Compare runner-reported config with server-approved policy.
- Quarantine drifted runners.
- Revoke active claim credentials.
- Emit audit events and operator tasks.

### Phase 5: Release and AI Workflows

- Add release-runner profile.
- Execute Java and Rust release build/test tasks through the runner.
- Add ConfigProfile manifest and `event-importer` dry-run tasks.
- Add AI failure analysis and bounded repair loops.
- Gate publish and signing tasks behind human approval and per-task isolation.

## Open Questions

- Should runner registration and task claim be direct HTTP APIs, WebSocket
  messages through `controller-rs`, or both?
- Where should long-running task logs and artifacts be stored for SaaS
  deployments?
- How should the control plane attest VM-based runners that do not have a
  container image digest?
- Should command templates be stored in workflow definitions, tenant policy,
  or a separate runner policy registry?
- How much of the existing `TaskExecutor` should move into shared crates so
  `light-workflow` and `light-workflow-runner` can share evaluation and result
  handling without sharing orchestration responsibilities?

## Recommendation

Create `light-workflow-runner` as a separate executable and keep
`light-workflow` as the single SaaS-owned orchestrator. The runner should be a
leased execution agent, not a workflow starter or workflow definition loader.

This gives tenants a practical way to run workflow tasks near their own APIs,
gateways, repositories, clusters, and sandboxes while keeping workflow start
events, policy decisions, task visibility, and audit under the SaaS control
plane.
