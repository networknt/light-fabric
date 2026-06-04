# Sandbox Execution

## Status

Proposed product design.

`light-workflow` should support sandbox-backed execution for tenant-authored and
automation-heavy workflows. The workflow engine remains the durable
orchestrator on the host. Sandboxes execute selected effectful tasks, or a
bounded group of related tasks, according to an approved execution security
profile.

Cube Sandbox is a good candidate provider for this boundary because it is
designed for fast VM-backed sandbox creation, hardware isolation, and network
policy enforcement. The design below treats Cube Sandbox as a pluggable
provider, not as a hard dependency in the workflow DSL.

## Problem

Workflows can be created by tenants and can eventually include tasks that run
commands, scripts, containers, model calls, MCP tools, browser automation, or
release automation. Those capabilities are useful, but they are also the
highest-risk part of the workflow runtime.

The platform needs a way to say:

- whether a workflow is allowed to use sandbox execution,
- which tasks must be sent to a sandbox,
- whether tasks should share a sandbox session,
- which network, filesystem, image, command, and secret policies apply,
- how release-style workflows can keep a workspace and cache across steps
  without moving the workflow orchestrator itself into the sandbox.

## Recommendation

Add an execution security profile to the workflow definition. The profile is a
request, not a final authority. At runtime, `light-workflow` computes an
effective profile from:

- workflow definition metadata,
- task metadata,
- tenant policy,
- service policy,
- operator-approved profile definitions,
- deployment defaults.

The workflow engine should stay on the host and continue to own task claiming,
context loading, branching, retries, persistence, and audit. Sandbox execution
should be delegated to a sandbox runner for the tasks that need isolation.

For release workflows, use one sandbox session per workflow instance by
default. That allows checkout state, build caches, generated artifacts, and
intermediate files to survive across related build and test tasks. Use a fresh
task sandbox for high-privilege publish or signing tasks if they require
release tokens or signing material.

## First Schema Surface

Use existing metadata fields first so the design can be implemented without an
immediate workflow-core schema break. `WorkflowDefinitionMetadata` already has
`document.metadata`, and every task has `metadata` through
`TaskDefinitionFields`.

Workflow-level example:

```yaml
document:
  dsl: "1.0.3"
  namespace: release
  name: light-fabric-polyrepo-release
  version: "1.0.0"
  metadata:
    lightWorkflow:
      security:
        executionProfile: release-sandbox
        sandbox:
          mode: workflow-session
          provider: cubesandbox
          template: light-fabric-release
          reuse: same-workflow-instance
          ttl: PT2H
          idleTimeout: PT10M
```

Task-level example:

```yaml
do:
  - publish-github-release:
      run:
        shell:
          command: ./release.sh
          arguments:
            - "${ .version }"
      metadata:
        lightWorkflow:
          security:
            sandbox:
              mode: per-task
              reason: release-token-isolation
            secrets:
              - github-release-token
```

Later, the runtime can normalize a first-class `security` field into the same
internal policy object:

```yaml
security:
  executionProfile: release-sandbox
  sandbox:
    mode: workflow-session
    provider: cubesandbox
    template: light-fabric-release
```

## Execution Modes

`none`

Trusted workflows run in the host executor. This mode should be limited to
internal workflows or workflows with no effectful untrusted task.

`effectful-tasks`

The default tenant mode. Pure orchestration tasks stay on the host, while
effectful tasks are delegated to sandbox execution. Examples include shell,
script, container, browser automation, external MCP servers, and filesystem
work.

`workflow-session`

One sandbox session is created for a workflow instance and reused by approved
tasks in that same instance. This is the right default for build, test, and
release workflows because the sandbox can keep cloned repositories,
dependency caches, build output, and temporary files across steps.

`per-task`

Each sandboxed task gets a fresh sandbox. This is the strongest isolation mode
and should be used for untrusted commands, tasks with separate privilege levels,
and tasks that receive high-value secrets.

## Task Routing

Host execution should remain the default for control-plane tasks:

```text
ask
assert
set
switch
workflow context merge
task claiming and completion
process state persistence
```

Sandbox execution should be required for high-risk task families:

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
```

Provider calls need policy-based routing:

```text
call.http      host or sandbox, depending on egress policy
call.jsonrpc   host or sandbox, depending on egress policy
call.mcp       host for approved gateway endpoints, sandbox for external servers
call.agent     host for bounded native model calls, sandbox when tools or code execution are enabled
call.rule      host unless a rule profile explicitly requires isolation
```

A task may request stricter isolation than the workflow profile, but it must
not weaken the effective profile. For example, a workflow can run in
`workflow-session` mode while a publish task requests `per-task` mode. A task
inside a tenant workflow cannot request `none` if the tenant ceiling requires
sandbox execution.

## Effective Policy

The runtime should compute and persist an effective policy for each workflow
instance:

```json
{
  "requestedProfile": "release-sandbox",
  "effectiveProfile": "release-sandbox",
  "sandboxMode": "workflow-session",
  "provider": "cubesandbox",
  "template": "light-fabric-release",
  "networkPolicy": "release-egress",
  "secretPolicy": "task-scoped",
  "approvedTaskTypes": ["run.shell", "call.http", "call.mcp"],
  "policyVersion": 7
}
```

This policy should be written into process audit metadata so replay and
incident review can prove which policy was active when the workflow ran.

Policy resolution rules:

- Tenant policy sets the maximum privilege a tenant can request.
- Service policy sets the maximum privilege `light-workflow` may grant in the
  current deployment.
- Workflow metadata requests a profile.
- Task metadata can request stricter handling.
- Runtime validation rejects unsupported or unapproved task/provider
  combinations before the task executes.
- Approval-required profile changes emit pending workflow-definition events and
  must not immediately publish an active workflow definition.

## Sandbox Session Lifecycle

For `workflow-session` mode:

1. Claim a task on the host.
2. Resolve the effective workflow security profile.
3. Create or resume the sandbox session for this workflow instance.
4. Mount or create an isolated workspace for the workflow instance.
5. Send the task input, command specification, environment allowlist, and
   permitted secret handles to the sandbox runner.
6. Stream logs and collect bounded output.
7. Copy declared artifacts to a controlled artifact store.
8. Return structured task output to `light-workflow`.
9. Update task and process state on the host.
10. Destroy the sandbox when the workflow completes, fails permanently, times
    out, or is cancelled.

The sandbox session id should be scoped to:

```text
tenant id
workflow definition id and version
workflow instance id
effective profile version
requesting principal
```

Do not reuse one sandbox across tenants, workflow definitions, unrelated
workflow instances, or different requesting principals.

## Release Workflow Example

A Light-Fabric release workflow can use one sandbox session to release these
repositories:

```text
light-fabric
portal-service
controller-rs
light-example-rs
```

The host `light-workflow` process should still own the workflow instance. The
sandbox holds the release workspace:

```text
light-workflow host
  - claims tasks
  - loads workflow context from Postgres
  - resolves policy
  - starts or resumes sandbox session
  - dispatches build/test/release commands
  - records task output, status, and audit

sandbox session
  - checks out repositories
  - runs tests and build scripts
  - stores dependency caches
  - produces release artifacts
  - exposes logs and declared artifacts
```

Recommended task grouping:

```text
prepare workspace          workflow-session sandbox
checkout repositories      workflow-session sandbox
run unit tests             workflow-session sandbox
build docker images        workflow-session sandbox, if Docker or BuildKit is available in policy
package release artifacts  workflow-session sandbox
generate release notes     workflow-session sandbox
publish release            per-task sandbox or isolated publish worker
sign artifacts             per-task sandbox or external signing service
```

The normal build/test/package tasks can share the same sandbox because they
belong to one workflow instance and benefit from shared workspace state. Publish
and signing tasks should be isolated because they require stronger secrets and
have irreversible external effects.

## Secret Handling

The sandbox should never receive broad platform credentials. Secrets must be:

- referenced by logical name in workflow or task metadata,
- approved by the effective profile,
- injected only for the task that needs them,
- short-lived where the provider supports it,
- redacted from logs and task output,
- excluded from workflow context exports.

Release tokens should be task-scoped. For example, tests and builds do not need
GitHub release credentials. The publish task can receive a short-lived release
token in a fresh sandbox or through a separate publish worker.

## Network Policy

Each profile should define egress explicitly. A release profile might allow:

```text
github.com
api.github.com
ghcr.io
crates.io
index.crates.io
registry.npmjs.org
docker.io
```

Tenant workflows should not get unrestricted network access. The sandbox
provider must enforce the egress policy, and `light-workflow` should still keep
its existing destination validation for host-executed HTTP, JSON-RPC, and MCP
calls.

## Artifact Boundary

The sandbox filesystem is not the workflow state store. Tasks must declare
which outputs are copied out:

```yaml
metadata:
  lightWorkflow:
    artifacts:
      - dist/*.tar.gz
      - dist/*.sha256
      - target/release/light-workflow
```

The runtime should copy artifacts into a controlled store and record artifact
metadata in task output:

```json
{
  "artifacts": [
    {
      "name": "light-fabric-0.3.0-x86_64-unknown-linux-gnu.tar.gz",
      "sha256": "...",
      "size": 12450000,
      "storeUri": "artifact://..."
    }
  ]
}
```

## Audit

Every sandboxed task should record:

- workflow definition id and version,
- workflow instance id,
- task id and task name,
- requested and effective profile,
- sandbox provider, template, session id, and sandbox id,
- command, argv, working directory, and environment allowlist,
- injected secret names, not values,
- network policy id,
- artifact metadata,
- exit status,
- duration,
- output size,
- log reference,
- policy version.

For `call: agent`, also record model provider, model name, prompt profile,
token budget, output schema id, validation result, and whether tool execution
was allowed.

## Failure Handling

Sandbox failures should map to normal workflow task failure semantics:

- startup failure: task fails with `sandbox_start_failed`,
- policy rejection: task fails with `sandbox_policy_denied`,
- timeout: task fails with `sandbox_timeout`,
- command non-zero exit: task fails with `command_failed`,
- oversized output: task fails with `sandbox_output_too_large`,
- sandbox lost: task fails or retries according to task retry policy.

The host must not mark a task complete until the sandbox result has been
validated and persisted. If a sandbox dies after a command has external side
effects, retries must respect the task idempotency key and release workflow
guardrails.

## Implementation Plan

1. Define `ExecutionSecurityProfile` and sandbox policy structs in
   `light-workflow`.
2. Parse workflow and task metadata under `lightWorkflow.security`.
3. Add effective-profile resolution using tenant and service ceilings.
4. Add a `SandboxRunner` trait with provider-neutral operations:
   create session, execute task, copy artifacts, checkpoint, destroy.
5. Add a Cube Sandbox provider implementation behind configuration.
6. Keep unsupported `run.*` task types disabled until they route through
   `SandboxRunner`.
7. Persist sandbox session metadata in process context or a dedicated runtime
   table.
8. Add audit output to every sandboxed task.
9. Add approval gates for profiles that enable command execution, external MCP
   servers, broad egress, or task-scoped secrets.
10. Add release workflow examples that use `workflow-session` mode for
    build/test/package and `per-task` mode for publish/sign.

## Open Decisions

- Whether sandbox profiles live only in service configuration or are also
  portal-managed records.
- Whether artifact storage should use portal tables, object storage, or both.
- Whether publish/sign should run in a sandbox or call a separate release
  service.
- How much of the Cube Sandbox API should be exposed directly versus hidden
  behind a provider-neutral interface.
- Whether a first-class `security` field should be added to `workflow-core`
  after the metadata-based design proves stable.

## References

- [Cube Sandbox Introduction](https://cubesandbox.com/guide/introduction.html)
- [Cube Sandbox Home](https://cubesandbox.com/)
