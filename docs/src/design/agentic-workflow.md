# Agentic Workflow Design

Agentic Workflow in Light-Fabric implements a hybrid orchestration model for enterprise business processes. The workflow is deterministic, auditable, and stateful, while selected steps can be executed by agents, API calls, rule engine checks, or humans.

The design goal is not to replace enterprise process control with an open-ended agent loop. The goal is to let agents work inside a managed process that has clear state, clear ownership, repeatable execution, and human approval where needed.

## Enterprise Challenge

In regulated or operationally sensitive environments, a purely autonomous AI agent is not enough for long-running business work.

- Compliance requires deterministic process paths, approval records, and audit history.
- Reliability requires long-running state to survive process restarts, UI disconnects, and agent failures.
- Safety requires human-in-the-loop checkpoints for decisions with business, security, or financial impact.
- Coordination requires multiple humans and roles to participate in the same process.
- Testing requires the same workflow to run interactively with humans or headlessly with example data.

Light-Fabric solves this by separating orchestration from execution.

## Hybrid Model

The workflow is the deterministic process manager. It defines the ordered steps, conditions, retries, error handling, human checkpoints, and outputs.

Agents are workers inside that process. They can reason, call tools, ask for missing data, and use skills, but they do not own the overall process state.

| Feature | Traditional Workflow | Pure Agent Loop | Light-Fabric Hybrid |
| :--- | :--- | :--- | :--- |
| Path | Fixed | Dynamic | Fixed path with flexible task execution |
| State | Durable | Often transient | Durable workflow and task state |
| Human input | Forms and approvals | Ad hoc chat | First-class waiting tasks |
| Audit | Strong | Weak | Step-level audit and agent trace |
| API calls | Built into code | Tool calls | Spec-described endpoint invocations |
| Testing | Separate test harness | Prompt replay | Same workflow can run live tests |

## Core Separation

There are two related specifications:

1. **Agentic Workflow Specification**
   Describes orchestration: task order, branching, human input, assertions, API calls, retries, errors, exports, and state transitions.

2. **LightAPI Description Specification**
   Describes API capabilities at the endpoint level: how an endpoint is invoked, what inputs it accepts, what result shape it returns, examples, behavior notes, and result expectations.

This separation is important. The workflow should not duplicate every endpoint contract. It should reference endpoint descriptions and use them to invoke calls, guide agents, and verify results.

## Endpoint-Level Consumption

Light-Portal manages API descriptions at the endpoint level, not only at the whole API level.

This is necessary because real workflows often combine one endpoint from one API with one endpoint from another API. For example, onboarding an API to an AI gateway may involve:

1. register an API
2. create an API version from a specification
3. create a development API instance
4. configure the API through config server
5. link the API instance to a gateway instance
6. select endpoints to expose as MCP tools
7. create a gateway config snapshot
8. reload the gateway through controller
9. run MCP tests against the gateway

Each step may come from a different API surface. The workflow consumes only the endpoints it needs.

The recommended model is:

- API-level descriptions can be authored for convenience and consistency.
- Endpoint-level descriptions are published and consumed by agents and workflows.
- Endpoint descriptions inherit shared context such as authentication, environments, sources, and secrets from an API catalog.
- Agents progressively load endpoint information by disclosure level instead of receiving the entire catalog up front.

## Progressive Disclosure

Endpoint descriptions should be disclosed to agents in layers:

- index: operation id, title, tags, visibility
- summary: purpose, capability group, lifecycle
- invocation: input shape, request mapping, auth, examples
- behavior: result cases, errors, edge cases, assertions
- full: complete description for debugging or generation

This allows the agent to discover capabilities cheaply, load invocation details only for selected endpoints, and load behavior details only when verification or failure analysis needs it.

## Workflow Task Types

The updated workflow specification adds first-class support for the task types needed by agentic API workflows.

### Ask Task

`ask` pauses the workflow and waits for human input. It supports prompts, choices, validation, defaults, timeouts, and sensitive input.

The task returns the user's answer as task output. The normal `export` block should move the answer into workflow context.

Example:

```yaml
- ask-authz:
    ask:
      prompt: Do you want to configure endpoint authorization?
      mode: choice
      options:
        - label: Configure authorization
          value: configure
        - label: Skip
          value: skip
    export:
      as:
        authzChoice: ${ .result }
```

### Assert Task

`assert` validates workflow state or API results. It is used for both live tests and interactive workflows.

It supports simple comparisons, JSONPath-style checks, length checks, regex checks, and rule-engine-backed assertions for complex business logic.

Assertion failures should produce structured, catchable errors so workflows can route failures to remediation, task creation, or agent investigation. Complex business assertions can delegate to [Light-Rule](light-rule.md).

### API Call Tasks

The workflow supports direct and description-backed API calls:

- HTTP / OpenAPI
- JSON-RPC
- OpenRPC
- gRPC
- MCP tool/resource/prompt calls

For direct internal calls, `jsonrpc` can be used with an endpoint, method, params, id, notification flag, and error policy.

For cataloged JSON-RPC, `openrpc` references an OpenRPC document and method.

For MCP, the workflow references a tool, resource, or prompt and passes arguments. MCP capability descriptions belong in the API description layer; the workflow only selects and invokes them.

### Explanation Metadata

Tasks can include `explain` metadata to help an agent or UI explain what is happening.

Useful fields include:

- purpose
- visible
- before
- success
- failure
- requires

Example:

```yaml
explain:
  purpose: Link the API instance to the development gateway.
  visible: true
  requires:
    - portal-command-token authentication
    - apiInstanceId from prior step
```

## Human Task State

Human-in-the-loop behavior must be represented as durable workflow state.

Recommended task states:

```text
A = active
W = waiting for input
C = completed
F = failed
X = canceled
```

When an `ask` or approval task reaches `W`, the process remains active but the task is no longer picked up by the executor. A user, CLI, scheduler, or agent must complete the task through the workflow API.

Waiting tasks should carry:

- prompt
- input mode
- options
- validation rules
- default value
- sensitive flag
- assignment metadata
- explanation metadata
- timeout policy

## Assignment And Worklist

Enterprise workflows need more than chat. Some tasks must be assigned to roles or users and coordinated across multiple humans.

Human tasks should support:

- assigned user
- assigned role
- candidate roles
- claimed by
- claimed timestamp
- due timestamp
- priority
- comments
- audit trail

A role-based task appears in the worklist for users with a matching role. Once claimed, it belongs to the claiming user until completed, released, delegated, or timed out.

## Client Architecture

`light-workflow` should run as a containerized backend service alongside other portal services. It owns workflow execution and state. Portal chat, worklist, CLI, scheduler, and agents are all clients of the same workflow APIs.

The client surfaces are:

- **Portal Chat**: conversational guidance for a single user.
- **Worklist**: role-based task inbox for approvals, reviews, and coordination.
- **CLI**: developer, CI/CD, live test, and automation interface.
- **Scheduler**: periodic headless execution, such as hourly live integration tests.
- **Agent**: task executor that can call APIs, use skills, and report results back to the workflow.

See [Workflow Client Architecture](workflow-client-architecture.md) for the dedicated client design.

## Workflow Service API

The workflow service should expose one stable API boundary for all clients.

Core operations:

```text
workflow.start
workflow.getInstance
workflow.listInstances
workflow.getEvents
workflow.listTasks
workflow.getTask
workflow.claimTask
workflow.releaseTask
workflow.completeTask
workflow.delegateTask
workflow.cancelInstance
```

Streaming clients should subscribe to workflow events through Server-Sent Events, WebSocket, or another portal-standard event mechanism.

Important event types:

- workflow started
- task started
- task completed
- task failed
- task waiting for input
- task assigned
- task claimed
- task completed by human
- agent started
- agent completed
- workflow completed
- workflow failed

## Live Testing

The same workflow runtime should support interactive runs and headless live tests.

Interactive workflows use `ask` tasks when decisions or missing values are needed.

Live tests should use example data from LightAPI endpoint descriptions and workflow input fixtures instead of asking the user. Assertions should verify results through `assert` tasks or rule-engine checks.

This lets the scheduler run workflows every hour against the latest deployed services. When a test fails, the workflow can create a task with the failure detail and assign an agent or human to investigate.

## Example: API Onboarding To AI Gateway

An API onboarding workflow can guide a user through a complex multi-endpoint process without requiring a dedicated UI for every operation.

The workflow can:

1. ask for or infer the API metadata
2. call the register API endpoint
3. create an API version from an OpenAPI specification
4. create a development API instance
5. configure the API
6. ask whether fine-grained authorization should be configured
7. route to create or select authorization rules
8. link the API instance to the development AI gateway
9. select endpoints to expose as MCP tools
10. create a gateway config snapshot
11. reload the gateway through controller
12. run MCP tests through the gateway
13. assert expected results
14. report success or create remediation tasks

The same workflow can run interactively through portal chat, be managed through the worklist, or run headlessly with examples as a live test.

## Technical Implementation

The Light-Fabric implementation is split across:

- `workflow-core`: Rust models for the workflow specification.
- `workflow-builder`: fluent builders for programmatic workflow construction.
- `light-workflow`: runtime service and executor.
- `light-agent`: agent execution surface for delegated agent tasks.
- `light-rule`: rule engine used by workflow and assertion tasks. See [Light-Rule Design](light-rule.md).

Runtime responsibilities include:

- deserializing workflow definitions
- claiming active tasks
- executing supported task types
- storing task output
- applying exports into process context
- creating next tasks
- pausing waiting tasks
- resuming after human completion
- failing or completing process instances
- exposing workflow APIs to clients

The current executable slice supports API invocation and verification tasks such as HTTP, JSON-RPC, OpenRPC, MCP over enterprise HTTP transports, rules, assertions, and waiting human input. MCP stdio transport is intentionally not a priority for enterprise deployment.

## Design Rule

There must be one workflow runtime and one task state model.

Chat, worklist, CLI, scheduler, and agents should never implement their own workflow execution. They should all use the same `light-workflow` service APIs.

This keeps enterprise workflow behavior auditable, testable, and consistent regardless of how a process is started, resumed, or observed.
